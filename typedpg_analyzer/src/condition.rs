//! Conditions the analyzer proves always or never true, for the lint.
//!
//! While [`crate::PgCatalog::query_view`] analyzes a query, every built-in
//! comparison (`=` `<>` `<` `<=` `>` `>=`) and `IN` list is judged with what
//! is known of its operands where it is evaluated: their type, nullability
//! and [`Refinement`] — the values and bounds a column's CHECK
//! constraints, its domain or enum, and the conditions holding there (an
//! earlier `AND` argument, an enclosing `WHERE`) leave it. A comparison
//! whose result is the same for every pair of non-NULL values the operands
//! can be is decided: never TRUE when it is FALSE for them all (a NULL
//! operand makes it NULL, not TRUE either), always TRUE when it is TRUE for
//! them all and neither operand can be NULL. A column compared with itself
//! is decided the same way (built-in equalities are reflexive).
//!
//! A decision is recorded per node location and AND-merged: a node
//! inferred more than once (a CTE read twice, a qual evaluated again) is
//! decided only when every inference agrees. Nothing is decided over a
//! column of a table with a `NOT VALID` CHECK constraint: rows older than
//! the constraint may hold any value.

use std::collections::BTreeSet;

use typedpg_pg_query::ast;

use crate::expr::ExprType;
use crate::oid::PgTypeOid;
use crate::pg_catalog::PgCatalog;
use crate::refine::{Exact, IntRange, Refinement};
use crate::refine_bounds::{OrdKind, OrdPoint, OrdRange};

/// A condition the analyzer proved constant where it is evaluated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionFact {
    /// Always TRUE (`true`), or never TRUE — FALSE or NULL (`false`).
    pub value: bool,
    /// Why.
    pub reason: ConditionReason,
}

/// Why a condition is constant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConditionReason {
    /// A column compared with itself (`x = x`); when always true, the
    /// column is never NULL there.
    SelfComparison,
    /// What one operand can be decides it: the left operand (of a
    /// comparison or an `IN`) when `left`, else the right one, is `known`
    /// (`"one of 'a', 'b'"`, `"at least 0"`); `source` names what says so
    /// when one constraint or domain alone does (`CHECK constraint
    /// "orders_status_check"`), else it is what holds there.
    Known {
        left: bool,
        known: String,
        source: Option<String>,
    },
}

thread_local! {
    /// How many synthetic comparisons (the rewrite of an `IN` list or a
    /// `BETWEEN`) are being inferred: those are not written nodes.
    static SYNTHETIC: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Run `f`, the inference of a synthetic comparison.
pub(crate) fn synthetic<R>(f: impl FnOnce() -> R) -> R {
    SYNTHETIC.with(|s| s.set(s.get() + 1));
    let out = f();
    SYNTHETIC.with(|s| s.set(s.get() - 1));
    out
}

fn in_synthetic() -> bool {
    SYNTHETIC.with(|s| s.get() > 0)
}

/// How the values compared are compared.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Equal values print the same (see [`Exact`]).
    Exact(Exact),
    /// In a total order with known bounds.
    Ord(OrdKind),
}

/// The comparison an operator name is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl Cmp {
    fn of(name: &str) -> Option<Cmp> {
        Some(match name {
            "=" => Cmp::Eq,
            "<>" | "!=" => Cmp::Ne,
            "<" => Cmp::Lt,
            "<=" => Cmp::Le,
            ">" => Cmp::Gt,
            ">=" => Cmp::Ge,
            _ => return None,
        })
    }

    /// The same comparison with its operands swapped.
    fn swapped(self) -> Cmp {
        match self {
            Cmp::Lt => Cmp::Gt,
            Cmp::Le => Cmp::Ge,
            Cmp::Gt => Cmp::Lt,
            Cmp::Ge => Cmp::Le,
            c => c,
        }
    }
}

/// Record the decision about the comparison `expr` (a written `l op r`)
/// whose operands are `left` and `right`, resolved to the built-in
/// operator taking `declared` operand types, compared under `collation`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn note_comparison(
    expr: &ast::AExpr,
    op_name: &str,
    builtin: bool,
    declared: (Option<PgTypeOid>, PgTypeOid),
    left: &ExprType,
    right: &ExprType,
    collation: Option<Option<crate::oid::PgCollationOid>>,
    ctx: crate::expr::Ctx<'_>,
) {
    if !crate::pg_catalog::collecting_column_ref_bindings() || in_synthetic() || expr.location < 0 {
        return;
    }
    let fact = (|| {
        let cmp = Cmp::of(op_name)?;
        if !builtin {
            return None;
        }
        let snapshot = ctx.snapshot;
        // A column compared with itself.
        if let (Some(ast::Node::ColumnRef(l)), Some(ast::Node::ColumnRef(r))) =
            (expr.lexpr.as_ref(), expr.rexpr.as_ref())
            && l.fields == r.fields
            && plain_scalar(left.type_oid, snapshot)
            && !unreliable_column(expr.lexpr.as_ref(), ctx)
        {
            let value = matches!(cmp, Cmp::Eq | Cmp::Le | Cmp::Ge);
            if value && left.nullable {
                return None;
            }
            return Some(ConditionFact {
                value,
                reason: ConditionReason::SelfComparison,
            });
        }
        let collation = collation?;
        let (tl, tr) = target_types(declared, left.type_oid, right.type_oid, snapshot)?;
        let kind = kind_of(tl, tr, collation, snapshot)?;
        let l = left.refine.converted(left.type_oid, tl, snapshot);
        let r = right.refine.converted(right.type_oid, tr, snapshot);
        let truth = decide(cmp, &l, &r, kind)?;
        if truth && (left.nullable || right.nullable) {
            return None;
        }
        // The operand the decision is about: the one that isn't a constant.
        let subject_left = !is_constant(expr.lexpr.as_ref()) || is_constant(expr.rexpr.as_ref());
        let (subject_node, subject, other, cmp_s) = if subject_left {
            (expr.lexpr.as_ref(), &l, &r, cmp)
        } else {
            (expr.rexpr.as_ref(), &r, &l, cmp.swapped())
        };
        if unreliable_column(expr.lexpr.as_ref(), ctx)
            || unreliable_column(expr.rexpr.as_ref(), ctx)
        {
            return None;
        }
        let known = describe(subject, kind, cmp_s)?;
        let target = if subject_left { tl } else { tr };
        let source = schema_source(subject_node, ctx, |schema| {
            let s = schema.converted(column_type(subject_node, ctx)?, target, snapshot);
            (decide(cmp_s, &s, other, kind) == Some(truth)).then_some(())
        });
        Some(ConditionFact {
            value: truth,
            reason: ConditionReason::Known {
                left: subject_left,
                known,
                source,
            },
        })
    })();
    crate::pg_catalog::note_condition_fact(expr.location, fact);
}

/// Record the decision about the `IN` list `expr` (`<>` for `NOT IN`):
/// `left` compared with each item's `items`, under `collation`.
pub(crate) fn note_in_list(
    expr: &ast::AExpr,
    negated: bool,
    left: &ExprType,
    items: &[&ExprType],
    collation: Option<Option<crate::oid::PgCollationOid>>,
    builtin: bool,
    ctx: crate::expr::Ctx<'_>,
) {
    if !crate::pg_catalog::collecting_column_ref_bindings() || in_synthetic() || expr.location < 0 {
        return;
    }
    let fact = (|| {
        if !builtin || items.is_empty() {
            return None;
        }
        let snapshot = ctx.snapshot;
        let collation = collation?;
        let target = snapshot.unwrap_domain(left.type_oid);
        if target == crate::pg_catalog::oid::UNKNOWN {
            return None;
        }
        let exact = Exact::of(target, collation, snapshot)?;
        if unreliable_column(expr.lexpr.as_ref(), ctx) {
            return None;
        }
        let l = left.refine.converted(left.type_oid, target, snapshot);
        let item_values: Vec<Option<BTreeSet<String>>> = items
            .iter()
            .map(|t| t.refine.converted(t.type_oid, target, snapshot).values)
            .collect();
        // The values each item certainly is: a non-NULL item of one value.
        let covered: BTreeSet<String> = items
            .iter()
            .zip(&item_values)
            .filter(|(t, _)| !t.nullable)
            .filter_map(|(_, vs)| match vs {
                Some(vs) if vs.len() == 1 => vs.iter().next().cloned(),
                _ => None,
            })
            .collect();
        let left_values = l.enumerated();
        // Every value the left side can be is certainly in the list.
        let all_covered = left_values
            .as_ref()
            .is_some_and(|lv| !lv.is_empty() && lv.iter().all(|v| covered.contains(v)));
        // No item can equal what the left side can be.
        let never_equal = item_values.iter().all(|vs| {
            vs.as_ref().is_some_and(|vs| {
                vs.iter().all(|v| {
                    left_values.as_ref().is_some_and(|lv| !lv.contains(v))
                        || (exact == Exact::Int
                            && l.int_bounds()
                                .zip(v.parse::<i128>().ok())
                                .is_some_and(|(b, i)| !b.contains(i)))
                })
            })
        });
        let value = if !negated {
            if never_equal {
                false
            } else if all_covered && !left.nullable {
                true
            } else {
                return None;
            }
        } else if all_covered {
            false
        } else if never_equal && !left.nullable && items.iter().all(|t| !t.nullable) {
            true
        } else {
            return None;
        };
        let known = describe(&l, Kind::Exact(exact), Cmp::Eq)?;
        let source = schema_source(expr.lexpr.as_ref(), ctx, |schema| {
            let s = schema.converted(column_type(expr.lexpr.as_ref(), ctx)?, target, snapshot);
            let sv = s.enumerated();
            let ok = if value != negated {
                sv.is_some_and(|sv| !sv.is_empty() && sv.iter().all(|v| covered.contains(v)))
            } else {
                item_values.iter().all(|vs| {
                    vs.as_ref().is_some_and(|vs| {
                        vs.iter()
                            .all(|v| sv.as_ref().is_some_and(|sv| !sv.contains(v)))
                    })
                })
            };
            ok.then_some(())
        });
        Some(ConditionFact {
            value,
            reason: ConditionReason::Known {
                left: true,
                known,
                source,
            },
        })
    })();
    crate::pg_catalog::note_condition_fact(expr.location, fact);
}

/// A scalar type a built-in equality is reflexive over: a base type, an
/// enum, or a domain over one — no array, composite, range or pseudo-type.
fn plain_scalar(t: PgTypeOid, snapshot: &PgCatalog) -> bool {
    use crate::pg_catalog::TypType;
    let base = snapshot.unwrap_domain(t);
    snapshot.pg_type.get(&base).is_some_and(|ty| {
        matches!(ty.typtype, TypType::Base | TypType::Enum) && ty.typelem.is_none()
    }) && crate::expr::array_element_type(snapshot, base).is_none()
}

/// The types the operator compares its operands as: its declared ones, or
/// for a polymorphic one (`anyenum`), the operands' own.
fn target_types(
    declared: (Option<PgTypeOid>, PgTypeOid),
    left: PgTypeOid,
    right: PgTypeOid,
    snapshot: &PgCatalog,
) -> Option<(PgTypeOid, PgTypeOid)> {
    use crate::pg_catalog::oid;
    let pseudo = |t: PgTypeOid| {
        snapshot
            .pg_type
            .get(&t)
            .is_some_and(|ty| ty.typtype == crate::pg_catalog::TypType::Pseudo)
    };
    let actual = if left == oid::UNKNOWN { right } else { left };
    if actual == oid::UNKNOWN {
        return None;
    }
    let pick = |d: PgTypeOid| {
        if pseudo(d) {
            snapshot.unwrap_domain(actual)
        } else {
            d
        }
    };
    Some((pick(declared.0?), pick(declared.1)))
}

/// How values of types `l` and `r` compare under `collation`.
fn kind_of(
    l: PgTypeOid,
    r: PgTypeOid,
    collation: Option<crate::oid::PgCollationOid>,
    snapshot: &PgCatalog,
) -> Option<Kind> {
    let (el, er) = (
        Exact::of(l, collation, snapshot),
        Exact::of(r, collation, snapshot),
    );
    if el.is_some() && el == er {
        return el.map(Kind::Exact);
    }
    let (ol, or) = (OrdKind::of(l, snapshot), OrdKind::of(r, snapshot));
    if ol.is_some() && ol == or {
        return ol.map(Kind::Ord);
    }
    None
}

/// The result of `l cmp r` for every pair of non-NULL values the two can
/// be, when it is the same for all of them.
fn decide(cmp: Cmp, l: &Refinement, r: &Refinement, kind: Kind) -> Option<bool> {
    let ranges = || -> Option<(OrdRange, OrdRange)> {
        match kind {
            Kind::Exact(Exact::Int) => Some((
                OrdRange::of_ints(&l.int_bounds()?),
                OrdRange::of_ints(&r.int_bounds()?),
            )),
            Kind::Ord(_) => Some((l.bounds?, r.bounds?)),
            Kind::Exact(_) => None,
        }
    };
    let eq = || -> Option<bool> {
        if let Kind::Exact(_) = kind
            && let (Some(lv), Some(rv)) = (l.enumerated(), r.enumerated())
        {
            if lv.is_disjoint(&rv) {
                return Some(false);
            }
            if lv.len() == 1 && lv == rv {
                return Some(true);
            }
        }
        let (a, b) = ranges()?;
        if a.meet(&b).is_empty() {
            return Some(false);
        }
        match (single(&a), single(&b)) {
            (Some(x), Some(y)) if x == y => Some(true),
            _ => None,
        }
    };
    match cmp {
        Cmp::Eq => eq(),
        Cmp::Ne => eq().map(|v| !v),
        Cmp::Lt | Cmp::Le | Cmp::Gt | Cmp::Ge => {
            let (a, b) = ranges()?;
            let (a, b, strict) = match cmp {
                Cmp::Lt => (a, b, true),
                Cmp::Le => (a, b, false),
                Cmp::Gt => (b, a, true),
                _ => (b, a, false),
            };
            // `a < b` (`a <= b` when not strict) for every pair, or for none.
            if below(&a, &b, strict) {
                Some(true)
            } else if below(&b, &a, !strict) {
                Some(false)
            } else {
                None
            }
        }
    }
}

/// The one point a range holds.
fn single(r: &OrdRange) -> Option<OrdPoint> {
    match (r.lo, r.hi) {
        (Some(lo), Some(hi)) if lo.point == hi.point && lo.inclusive && hi.inclusive => {
            Some(lo.point)
        }
        _ => None,
    }
}

/// Whether every value of `a` is below (`strict`) or at most every value
/// of `b`.
fn below(a: &OrdRange, b: &OrdRange, strict: bool) -> bool {
    let (Some(hi), Some(lo)) = (a.hi, b.lo) else {
        return false;
    };
    if hi.point < lo.point {
        return true;
    }
    hi.point == lo.point && (!strict || !(hi.inclusive && lo.inclusive))
}

/// What `r` says, in words: `one of 'a', 'b'`, `at least 0`, … — what the
/// decision of `cmp` read.
fn describe(r: &Refinement, kind: Kind, cmp: Cmp) -> Option<String> {
    let quote = |v: &str| match kind {
        Kind::Exact(Exact::Text) => format!("'{}'", v.replace('\'', "''")),
        _ => v.to_owned(),
    };
    let equality = matches!(cmp, Cmp::Eq | Cmp::Ne);
    if equality
        && let Kind::Exact(_) = kind
        && let Some(vs) = r.enumerated()
    {
        let mut vs: Vec<String> = vs.iter().map(|v| quote(v)).collect();
        if kind == Kind::Exact(Exact::Int) {
            vs.sort_by_key(|v| v.parse::<i128>().unwrap_or(i128::MAX));
        }
        return Some(match vs.as_slice() {
            [] => "never a value".to_owned(),
            [v] => format!("always {v}"),
            _ => {
                const SHOWN: usize = 8;
                let more = vs.len().saturating_sub(SHOWN);
                vs.truncate(SHOWN);
                let mut list = vs.join(", ");
                if more > 0 {
                    list.push_str(&format!(", … ({more} more)"));
                }
                format!("always one of {list}")
            }
        });
    }
    match kind {
        Kind::Exact(Exact::Int) => {
            let b = r.int_bounds()?;
            Some(int_words(&b))
        }
        Kind::Ord(ord) => {
            // (A `date` or timestamp decided otherwise than by its bounds,
            // its finiteness: said vaguely.)
            let vague = || {
                (ord != OrdKind::Numeric)
                    .then(|| "always within the bounds known for it".to_owned())
            };
            let Some(b) = r.bounds.filter(|b| b.kind == ord) else {
                return vague();
            };
            let point = |p: OrdPoint| match (p, ord) {
                (OrdPoint::Finite(n), OrdKind::Numeric) => Some(n.to_string()),
                (OrdPoint::Finite(n), _) => datetime_words(n.to_integer()?, ord),
                (OrdPoint::NegInfinity, OrdKind::Numeric) => Some("-Infinity".to_owned()),
                (OrdPoint::Infinity, OrdKind::Numeric) => Some("Infinity".to_owned()),
                (OrdPoint::NegInfinity, _) => Some("-infinity".to_owned()),
                (OrdPoint::Infinity, _) => Some("infinity".to_owned()),
                (OrdPoint::NaN, _) => Some("NaN".to_owned()),
            };
            let point = |p: OrdPoint| point(p).unwrap_or_else(|| "a bound".to_owned());
            Some(match (b.lo, b.hi) {
                (Some(lo), Some(hi)) if lo.point == hi.point => {
                    format!("always {}", point(lo.point))
                }
                (Some(lo), Some(hi)) => format!(
                    "always {} {} and {} {}",
                    if lo.inclusive { "at least" } else { "above" },
                    point(lo.point),
                    if hi.inclusive { "at most" } else { "below" },
                    point(hi.point)
                ),
                (Some(lo), None) => format!(
                    "always {} {}",
                    if lo.inclusive { "at least" } else { "above" },
                    point(lo.point)
                ),
                (None, Some(hi)) => format!(
                    "always {} {}",
                    if hi.inclusive { "at most" } else { "below" },
                    point(hi.point)
                ),
                (None, None) => return vague(),
            })
        }
        Kind::Exact(_) => None,
    }
}

/// A finite `date` (`v` days from 2000-01-01), `timestamp` or
/// `timestamptz` (`v` microseconds from 2000-01-01 00:00, UTC for a
/// `timestamptz`) in words, as the ISO `DateStyle` prints it — a
/// `timestamptz` in UTC (`+00`), a year before 1 as `BC`. `None` for
/// another kind.
fn datetime_words(v: i128, kind: OrdKind) -> Option<String> {
    const USECS_PER_DAY: i128 = 86_400_000_000;
    let (days, usecs) = match kind {
        OrdKind::Date => (v, None),
        OrdKind::Timestamp | OrdKind::TimestampTz => (
            v.div_euclid(USECS_PER_DAY),
            Some(v.rem_euclid(USECS_PER_DAY)),
        ),
        OrdKind::Numeric => return None,
    };
    // Days from 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 10_957 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i128::from(month <= 2);
    let date = if year <= 0 {
        format!("{:04}-{month:02}-{day:02} BC", 1 - year)
    } else {
        format!("{year:04}-{month:02}-{day:02}")
    };
    let Some(usecs) = usecs else {
        return Some(date);
    };
    let secs = usecs / 1_000_000;
    let fraction = usecs % 1_000_000;
    let mut time = format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
    if fraction != 0 {
        time.push_str(format!(".{fraction:06}").trim_end_matches('0'));
    }
    let zone = if kind == OrdKind::TimestampTz {
        "+00"
    } else {
        ""
    };
    // PostgreSQL puts the era after the time.
    Some(match date.strip_suffix(" BC") {
        Some(date) => format!("{date} {time}{zone} BC"),
        None => format!("{date} {time}{zone}"),
    })
}

fn int_words(b: &IntRange) -> String {
    match (b.lo, b.hi) {
        (Some(lo), Some(hi)) if lo == hi => format!("always {lo}"),
        (Some(lo), Some(hi)) => format!("always between {lo} and {hi}"),
        (Some(lo), None) => format!("always at least {lo}"),
        (None, Some(hi)) => format!("always at most {hi}"),
        (None, None) => "unbounded".to_owned(),
    }
}

/// Whether `node` is a constant: a literal, possibly cast, or a parameter.
fn is_constant(node: Option<&ast::Node>) -> bool {
    match node {
        Some(ast::Node::AConst(_) | ast::Node::ParamRef(_)) => true,
        Some(ast::Node::TypeCast(c)) => is_constant(c.arg.as_ref()),
        _ => false,
    }
}

/// The base-table column a plain column reference reads: its relation and
/// the column's attribute.
fn base_column(
    node: Option<&ast::Node>,
    ctx: crate::expr::Ctx<'_>,
) -> Option<(crate::oid::PgClassOid, crate::pg_catalog::PgAttribute)> {
    let Some(ast::Node::ColumnRef(c)) = node else {
        return None;
    };
    let parts = crate::expr::extract_string_fields(&c.fields);
    if parts.len() != c.fields.len() {
        return None;
    }
    let (table, column) = match parts.as_slice() {
        [col] => (None, col.as_str()),
        [tbl, col] => (Some(tbl.as_str()), col.as_str()),
        [_schema, tbl, col] => (Some(tbl.as_str()), col.as_str()),
        _ => return None,
    };
    let col = ctx.scope.resolve_column(table, column, None).ok()?;
    let (relid, name) = match ctx.scope.column_source(col).and_then(|s| s.relid) {
        Some(relid) => (relid, col.name.clone()),
        None => {
            let o = col.origin.as_ref()?;
            (o.relid, o.column.clone())
        }
    };
    let attr = ctx
        .snapshot
        .attributes_of(relid)
        .iter()
        .find(|a| a.attname == name)?
        .clone();
    Some((relid, attr))
}

fn column_type(node: Option<&ast::Node>, ctx: crate::expr::Ctx<'_>) -> Option<PgTypeOid> {
    base_column(node, ctx).map(|(_, a)| a.atttypid)
}

/// Whether `node` reads a column of a table with a `NOT VALID` CHECK
/// constraint, whose old rows may break it.
fn unreliable_column(node: Option<&ast::Node>, ctx: crate::expr::Ctx<'_>) -> bool {
    let Some((relid, _)) = base_column(node, ctx) else {
        return false;
    };
    ctx.snapshot.pg_constraint.values().any(|c| {
        c.conrelid == relid && c.contype == crate::pg_catalog::ConType::Check && !c.convalidated
    })
}

/// What alone decides the condition, as `decides` says of a refinement of
/// the base-table column `node` reads: one of its table's CHECK
/// constraints, or its domain.
fn schema_source(
    node: Option<&ast::Node>,
    ctx: crate::expr::Ctx<'_>,
    decides: impl Fn(&Refinement) -> Option<()>,
) -> Option<String> {
    let (relid, attr) = base_column(node, ctx)?;
    // An enum's labels, which no constraint needs to say.
    let base = ctx.snapshot.unwrap_domain(attr.atttypid);
    if let Some(ty) = ctx
        .snapshot
        .pg_type
        .get(&base)
        .filter(|ty| ty.typtype == crate::pg_catalog::TypType::Enum)
    {
        let labels = Refinement {
            values: Some(
                ctx.snapshot
                    .enum_labels_of(base)
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
            ),
            ..Refinement::NONE
        };
        if decides(&labels).is_some() {
            return Some(format!("enum \"{}\"", ty.typname));
        }
    }
    for (name, r) in ctx
        .snapshot
        .column_check_refinements(relid, &attr.attname, false)
    {
        if decides(&r).is_some() {
            return Some(format!("CHECK constraint \"{name}\""));
        }
    }
    let domain = ctx.snapshot.domain_refinement(attr.atttypid);
    if domain != Refinement::NONE && decides(&domain).is_some() {
        let name = ctx
            .snapshot
            .pg_type
            .get(&attr.atttypid)
            .map(|t| t.typname.clone())?;
        return Some(format!("domain \"{name}\""));
    }
    None
}

impl PgCatalog {
    /// What each enforced CHECK constraint of relation `relid` allows its
    /// column `column` when non-NULL, by constraint name (in name order) —
    /// the values of an `IN` list, the bounds of comparisons with
    /// constants (see `nonnull::checks::allowed_values`). `NOT VALID` ones
    /// only `with_not_valid`; a `NO INHERIT` one only when the relation has
    /// no children (a scan returns theirs too). Constraints that say
    /// nothing of the column are left out.
    pub(crate) fn column_check_refinements(
        &self,
        relid: crate::oid::PgClassOid,
        column: &str,
        with_not_valid: bool,
    ) -> Vec<(String, Refinement)> {
        let Some(attr) = self
            .attributes_of(relid)
            .iter()
            .find(|a| a.attname == column)
        else {
            return Vec::new();
        };
        let has_children = self.pg_inherits.iter().any(|i| i.inhparent == relid);
        let mut constraints: Vec<&crate::pg_catalog::PgConstraint> = self
            .pg_constraint
            .values()
            .filter(|c| {
                c.conrelid == relid
                    && c.contype == crate::pg_catalog::ConType::Check
                    && c.conenforced
                    && (with_not_valid || c.convalidated)
            })
            .collect();
        constraints.sort_by(|a, b| a.conname.cmp(&b.conname));
        let mut out = Vec::new();
        for con in constraints {
            let Some(def) = self.check_defs.get(&con.oid) else {
                continue;
            };
            if def.no_inherit && has_children {
                continue;
            }
            let (crate::ddl::tables::check_inherit::StoredExpr::Written(expr), Some(trust)) =
                (&def.expr, &def.trusted)
            else {
                continue;
            };
            if let Some(r) = crate::nonnull::checks::allowed_values(
                (**expr).as_ref(),
                column,
                attr.atttypid,
                attr.attcollation,
                self,
                trust,
            ) && r != Refinement::NONE
            {
                out.push((con.conname.clone(), r));
            }
        }
        out
    }
}

/// A constant an INSERT or UPDATE stores that a CHECK rejects: the
/// statement fails for every row it writes (see
/// [`PgCatalog::assignment_violation`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignmentViolation {
    /// PostgreSQL's error.
    pub message: String,
    /// The relation's CHECK constraint that fails (`None`: a domain's).
    pub constraint: Option<String>,
    /// What that constraint allows the column, in words (`"one of 'a',
    /// 'b'"`), when the analyzer reads it.
    pub allowed: Option<String>,
}

impl PgCatalog {
    /// The error storing the constant `value` (a literal, or one cast to
    /// the column's type) into column `column` of relation `relid` raises
    /// for every row an INSERT (`update` false) or an UPDATE writes: a
    /// CHECK constraint of the relation — `NOT VALID` ones included, they
    /// bind new rows — or of the column's domain is FALSE for it, whatever
    /// the row's other columns. `None` when that isn't certain (a rule or
    /// a BEFORE ROW trigger may change the row, a type modifier the value,
    /// the relation is a view, …).
    pub fn assignment_violation(
        &self,
        relid: crate::oid::PgClassOid,
        column: &str,
        value: &ast::Node,
        update: bool,
    ) -> Option<AssignmentViolation> {
        let (message, constraint) =
            crate::resolve::assignment_violation(self, relid, column, value, update)?;
        let allowed = constraint.as_ref().and_then(|name| {
            let attr = self
                .attributes_of(relid)
                .iter()
                .find(|a| a.attname == column)?;
            let (_, r) = self
                .column_check_refinements(relid, column, true)
                .into_iter()
                .find(|(n, _)| n == name)?;
            let kind = match Exact::of(attr.atttypid, attr.attcollation, self) {
                Some(e) => Kind::Exact(e),
                None => Kind::Ord(OrdKind::of(attr.atttypid, self)?),
            };
            let text = describe(&r, kind, Cmp::Eq)?;
            Some(text.strip_prefix("always ").unwrap_or(&text).to_owned())
        });
        Some(AssignmentViolation {
            message,
            constraint,
            allowed,
        })
    }
}

/// A constraint every value written into a column must satisfy: a CHECK
/// constraint of its relation that reads it, or a CHECK of its domain (see
/// [`PgCatalog::column_write_constraints`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteConstraint {
    /// The constraint, as a message names it: `CHECK constraint "c"`, or
    /// `CHECK constraint "c" of domain "d"`.
    pub source: String,
    /// The relation's columns it reads (a domain's: the column alone).
    pub columns: Vec<String>,
    /// What it allows the column when non-NULL, when the analyzer reduces
    /// it to the column alone (`CHECK (kind IN ('a', 'b'))`, `CHECK (n >
    /// 0)`); `None` for a constraint that says nothing it can read of the
    /// column by itself (`CHECK (a < b)`, `CHECK (length(name) > 2)`).
    pub allowed: Option<Refinement>,
    /// Its top-level AND conjuncts, each read for the column.
    pub parts: Vec<CheckPart>,
    /// A relation's CHECK not validated (`NOT VALID`): rows already stored
    /// may break it, and an UPDATE of one fails even when it doesn't write
    /// the columns the constraint reads. (A domain's isn't checked again
    /// on a value an UPDATE keeps.)
    pub not_valid: bool,
}

/// One top-level AND conjunct of a [`WriteConstraint`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckPart {
    /// The conjunct, normalized: the same for two conjuncts PostgreSQL's
    /// `equal()` finds equal (source locations aside), whatever constraint
    /// they are in or its name — a domain's apart from a relation's.
    pub key: String,
    /// The relation's columns it reads (a domain's: the column alone).
    pub columns: Vec<String>,
    /// What it says of the column.
    pub reading: CheckReading,
}

/// What a [`CheckPart`] says of the column a value is written into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckReading {
    /// Exactly: the value is NULL or one this refinement allows
    /// (`kind IN ('a', 'b')`, `n > 0 AND n <= 10`, `isfinite(at)`).
    Allows(Box<Refinement>),
    /// Exactly: the value is not NULL (`c IS NOT NULL`).
    NotNull,
    /// Something the analyzer can't read exactly of the column alone
    /// (`length(name) > 2`, `lo <= hi`).
    Other,
}

/// What a [`CheckPart`] evaluates to over the rows a statement writes (see
/// [`PgCatalog::check_part_outcome`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartOutcome {
    /// TRUE or NULL for every row: the rows pass it.
    Passes,
    /// FALSE for every row: every row written fails it.
    Fails,
    /// Not known, or TRUE for some rows and FALSE for others.
    Unknown,
}

/// The most combinations of the constants a row may hold that
/// [`PgCatalog::check_part_outcome`] evaluates a conjunct over.
const MAX_COMBINATIONS: usize = 64;

/// What a data-modifying statement of a query writes (see
/// [`crate::QueryView::writes`]).
#[derive(Debug, Clone, PartialEq)]
pub struct WriteStatement {
    pub command: WriteCommand,
    /// The relation it writes, as the analyzer resolved it.
    pub relid: crate::oid::PgClassOid,
    /// The values it writes into the relation's columns, in the order they
    /// were analyzed: one per VALUES row and per assignment.
    pub columns: Vec<WrittenColumn>,
    /// How many target lists the values were written by (see
    /// [`WrittenColumn::row`]).
    pub rows: usize,
    /// When `relid` is an automatically updatable view (through a chain of
    /// them): the table the rows are stored in. `None` for a table, and
    /// for a view whose writes for the statement's command are something
    /// else — a DO INSTEAD rule or an INSTEAD OF trigger for it on the
    /// view, on a view below it or (a rule) on the table; a MERGE through
    /// any rule.
    pub base: Option<WriteBase>,
}

/// The table a write through an automatically updatable view stores its
/// rows in (see [`WriteStatement::base`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteBase {
    /// The base table.
    pub relid: crate::oid::PgClassOid,
    /// Each column of the view the base table stores as is, with the base
    /// column it stores it in: `(view column, base column)`, in the view's
    /// column order.
    pub columns: Vec<(String, String)>,
    /// The base columns an INSERT giving no value for gets a value from a
    /// view of the chain instead of the base column's own default: a view
    /// column storing it has a default, its own or its domain's —
    /// PostgreSQL's rewriteTargetListIU fills each view's defaults in on
    /// the way down (`build_column_default`).
    pub defaulted: Vec<String>,
}

/// The command of a [`WriteStatement`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteCommand {
    Insert,
    Update,
    Merge,
}

/// A value a [`WriteStatement`] writes into a column.
#[derive(Debug, Clone, PartialEq)]
pub struct WrittenColumn {
    pub column: String,
    /// Written by an UPDATE: an UPDATE's SET, an `ON CONFLICT DO UPDATE
    /// SET`, a MERGE's `UPDATE SET`. Otherwise an inserted row's.
    pub update: bool,
    /// The target list that writes it, numbered from 0 in the statement:
    /// a VALUES row, an `INSERT … SELECT`'s output, an UPDATE's (or an `ON
    /// CONFLICT DO UPDATE`'s) SET list, a MERGE action. The values of one
    /// target list are written into one row together.
    pub row: usize,
    pub value: WrittenValue,
}

/// How a [`WrittenColumn`]'s value is given.
#[derive(Debug, Clone, PartialEq)]
pub enum WrittenValue {
    /// A bare parameter: its number in [`crate::QueryView::sql`] (`$n`).
    Parameter(i32),
    /// `DEFAULT`.
    Default,
    /// A value stored inside the column (`arr[1] = …`, `p.x = …`).
    Inside,
    /// Any other expression: its type and type modifier, whether it may be
    /// NULL, and what each non-NULL value is.
    Expression {
        type_oid: PgTypeOid,
        typmod: Option<i32>,
        nullable: bool,
        refinement: Box<Refinement>,
    },
}

/// The column names `n` reads (bare or qualified), those `known` accepts,
/// each once.
fn read_columns(n: Option<&ast::Node>, known: impl Fn(&str) -> bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (node, _) in n.into_iter().flat_map(ast::Node::nodes) {
        if let typedpg_pg_query::NodeRef::ColumnRef(c) = node
            && let Some(name) = crate::expr::extract_string_fields(&c.fields).last()
            && known(name)
            && !out.contains(name)
        {
            out.push(name.clone());
        }
    }
    out
}

/// The normalized key of `n` (see [`CheckPart::key`]), prefixed by `kind`.
fn node_key(kind: &str, n: Option<&ast::Node>) -> String {
    use typedpg_pg_query::Equal;
    let mut key = kind.to_owned();
    n.cloned().equal_key(&mut key);
    key
}

/// A CHECK expression's schema: the catalog, the relation its columns are
/// of and the nodes its resolution trusted.
struct Side<'a> {
    catalog: &'a PgCatalog,
    relid: crate::oid::PgClassOid,
    trust: &'a crate::nonnull::TrustedNodes,
}

/// Whether CHECK expression `old` not being FALSE makes `new` not FALSE
/// either (see [`PgCatalog::check_part_implied_by`]).
fn weakly_implies(
    old: (Option<&ast::Node>, &Side<'_>),
    new: (Option<&ast::Node>, &Side<'_>),
) -> bool {
    let constant = |n: Option<&ast::Node>| match n {
        // A missing ELSE is NULL.
        None => Some(None),
        Some(ast::Node::AConst(c)) if c.isnull => Some(None),
        Some(ast::Node::AConst(c)) => match &c.val {
            Some(ast::AConstValue::Boolean(b)) => Some(Some(b.boolval)),
            _ => None,
        },
        _ => None,
    };
    // `new` always passes; `old` never does.
    if matches!(constant(new.0), Some(None | Some(true))) || constant(old.0) == Some(Some(false)) {
        return true;
    }
    if node_key("", old.0) == node_key("", new.0) {
        return true;
    }
    // Three-valued `AND` is FALSE exactly when an operand is, and `OR` when
    // every operand is. So a new `AND` is not FALSE exactly when each of
    // its operands isn't, and an old `OR` is not FALSE exactly when one of
    // its operands isn't: both split exactly. An old `AND` not FALSE makes
    // each of its operands not FALSE, and a new `OR` is not FALSE once one
    // of its operands isn't: dropping a conjunct, adding a disjunct, or
    // reordering either widens nothing.
    fn operands(
        n: Option<&ast::Node>,
        op: ast::BoolExprType,
    ) -> Option<std::slice::Iter<'_, Option<ast::Node>>> {
        match n {
            Some(ast::Node::BoolExpr(b)) if b.boolop == op => Some(b.args.iter()),
            _ => None,
        }
    }
    if let Some(mut args) = operands(new.0, ast::BoolExprType::AndExpr) {
        return args.all(|arg| weakly_implies(old, (arg.as_ref(), new.1)));
    }
    if let Some(mut args) = operands(old.0, ast::BoolExprType::OrExpr) {
        return args.all(|arg| weakly_implies((arg.as_ref(), old.1), new));
    }
    if let Some(mut args) = operands(old.0, ast::BoolExprType::AndExpr)
        && args.any(|arg| weakly_implies((arg.as_ref(), old.1), new))
    {
        return true;
    }
    if let Some(mut args) = operands(new.0, ast::BoolExprType::OrExpr)
        && args.any(|arg| weakly_implies(old, (arg.as_ref(), new.1)))
    {
        return true;
    }
    let (Some(old_case), Some(new_case)) = (case_arms(old.0, old.1), case_arms(new.0, new.1))
    else {
        return false;
    };
    if old_case.subject != new_case.subject || old_case.exact != new_case.exact {
        return false;
    }
    fn branch<'n>(case: &CaseArms<'n>, value: &str) -> Option<&'n ast::Node> {
        case.arms
            .iter()
            .find(|(v, _)| v == value)
            .map_or(case.otherwise, |(_, body)| *body)
    }
    let values: BTreeSet<&str> = old_case
        .arms
        .iter()
        .chain(&new_case.arms)
        .map(|(v, _)| v.as_str())
        .collect();
    values.into_iter().all(|value| {
        weakly_implies(
            (branch(&old_case, value), old.1),
            (branch(&new_case, value), new.1),
        )
    }) && weakly_implies((old_case.otherwise, old.1), (new_case.otherwise, new.1))
}

/// A `CASE` over one column, each `WHEN` a constant the column equals.
struct CaseArms<'a> {
    /// The column's name.
    subject: String,
    /// How its values print, the same for equal ones.
    exact: Exact,
    /// Each constant (as the column's type reads it) with its body, the
    /// first `WHEN` of a constant only (a later one is never taken).
    arms: Vec<(String, Option<&'a ast::Node>)>,
    /// The `ELSE` (`None`: NULL).
    otherwise: Option<&'a ast::Node>,
}

/// `n` read as [`CaseArms`]: a simple `CASE col WHEN c THEN …` or a
/// searched `CASE WHEN col = c THEN …`, every comparison a built-in one of
/// a column of `side`'s relation whose type's equal values print the same.
fn case_arms<'a>(n: Option<&'a ast::Node>, side: &Side<'_>) -> Option<CaseArms<'a>> {
    use crate::nonnull::{StrictNode, Trust};
    let Some(ast::Node::CaseExpr(case)) = n else {
        return None;
    };
    let column_name = |n: Option<&ast::Node>| match n {
        Some(ast::Node::ColumnRef(c)) => crate::expr::extract_string_fields(&c.fields)
            .last()
            .cloned(),
        _ => None,
    };
    let literal = |n: Option<&ast::Node>| match n {
        Some(ast::Node::AConst(c)) if !c.isnull => match c.val.as_ref()? {
            ast::AConstValue::Integer(i) => Some(i.ival.to_string()),
            ast::AConstValue::String(s) => Some(s.sval.clone()),
            _ => None,
        },
        _ => None,
    };
    let mut subject: Option<String> = column_name(case.arg.as_ref());
    if case.arg.is_some() && subject.is_none() {
        return None;
    }
    let mut constants = Vec::new();
    for when in &case.args {
        let Some(ast::Node::CaseWhen(when)) = when else {
            return None;
        };
        let (column, constant) = if case.arg.is_some() {
            // A simple CASE's `test = value` is recorded at its WHEN.
            if !side.trust.trusts(when.location, StrictNode::BuiltinCompare) {
                return None;
            }
            (subject.clone(), literal(when.expr.as_ref()))
        } else {
            let Some(ast::Node::AExpr(cmp)) = when.expr.as_ref() else {
                return None;
            };
            if cmp.kind != ast::AExprKind::AexprOp
                || crate::expr::extract_string_fields(&cmp.name) != ["="]
                || !side.trust.trusts(cmp.location, StrictNode::BuiltinCompare)
            {
                return None;
            }
            match (
                column_name(cmp.lexpr.as_ref()),
                column_name(cmp.rexpr.as_ref()),
            ) {
                (Some(column), None) => (Some(column), literal(cmp.rexpr.as_ref())),
                (None, Some(column)) => (Some(column), literal(cmp.lexpr.as_ref())),
                _ => return None,
            }
        };
        let (Some(column), Some(constant)) = (column, constant) else {
            return None;
        };
        match &subject {
            Some(s) if *s != column => return None,
            Some(_) => {}
            None => subject = Some(column),
        }
        constants.push((constant, when.result.as_ref()));
    }
    let subject = subject?;
    let attr = side
        .catalog
        .attributes_of(side.relid)
        .iter()
        .find(|a| a.attnum > 0 && a.attname == subject)?;
    let exact = Exact::of(attr.atttypid, attr.attcollation, side.catalog)?;
    let mut arms: Vec<(String, Option<&ast::Node>)> = Vec::new();
    for (constant, body) in constants {
        let value = exact.read(&constant)?;
        if !arms.iter().any(|(v, _)| *v == value) {
            arms.push((value, body));
        }
    }
    Some(CaseArms {
        subject,
        exact,
        arms,
        otherwise: case.defresult.as_ref(),
    })
}

impl PgCatalog {
    /// The constraints a value an INSERT or UPDATE stores into column
    /// `column` of relation `relid` must satisfy: its enforced CHECK
    /// constraints that read the column — `NOT VALID` and `NO INHERIT` ones
    /// included, they bind the rows written into the relation itself — and
    /// the CHECK constraints of the column's domain (and of the domains it
    /// is over). In name order, the relation's first.
    pub fn column_write_constraints(
        &self,
        relid: crate::oid::PgClassOid,
        column: &str,
    ) -> Vec<WriteConstraint> {
        let attrs = self.attributes_of(relid);
        let Some(attr) = attrs.iter().find(|a| a.attname == column && a.attnum > 0) else {
            return Vec::new();
        };
        let mut constraints: Vec<&crate::pg_catalog::PgConstraint> = self
            .pg_constraint
            .values()
            .filter(|c| {
                c.conrelid == relid
                    && c.contype == crate::pg_catalog::ConType::Check
                    && c.conenforced
                    && c.conkey.contains(&attr.attnum)
            })
            .collect();
        constraints.sort_by(|a, b| a.conname.cmp(&b.conname));
        let is_column = |name: &str| attrs.iter().any(|a| a.attnum > 0 && a.attname == name);
        let mut out = Vec::new();
        for con in constraints {
            let columns = con
                .conkey
                .iter()
                .filter_map(|n| attrs.iter().find(|a| a.attnum == *n))
                .map(|a| a.attname.clone())
                .collect();
            let written =
                self.check_defs
                    .get(&con.oid)
                    .and_then(|def| match (&def.expr, &def.trusted) {
                        (
                            crate::ddl::tables::check_inherit::StoredExpr::Written(expr),
                            Some(trust),
                        ) => Some(((**expr).as_ref(), trust)),
                        _ => None,
                    });
            let allowed = written.and_then(|(expr, trust)| {
                let mut r = crate::nonnull::checks::allowed_values(
                    expr,
                    column,
                    attr.atttypid,
                    attr.attcollation,
                    self,
                    trust,
                )
                .unwrap_or_default();
                if crate::nonnull::checks::finite_columns_of(expr, trust)
                    .iter()
                    .any(|c| c == column)
                {
                    r.finite = true;
                }
                (r != Refinement::NONE).then_some(r)
            });
            let parts = match written {
                Some((expr, trust)) => crate::nonnull::checks::check_conjuncts(expr)
                    .into_iter()
                    .map(|n| CheckPart {
                        key: node_key("relation:", n),
                        columns: read_columns(n, is_column),
                        reading: crate::nonnull::checks::read_conjunct(
                            n,
                            column,
                            attr.atttypid,
                            attr.attcollation,
                            self,
                            trust,
                        ),
                    })
                    .collect(),
                // Not resolved: one part nothing can be read of, known by
                // the constraint's name.
                None => vec![CheckPart {
                    key: format!("unread:{}", con.conname),
                    columns: vec![column.to_owned()],
                    reading: CheckReading::Other,
                }],
            };
            out.push(WriteConstraint {
                source: format!("CHECK constraint \"{}\"", con.conname),
                columns,
                allowed,
                parts,
                not_valid: !con.convalidated,
            });
        }
        for domain in self.domain_chain(attr.atttypid) {
            let name = self
                .pg_type
                .get(&domain)
                .map(|t| t.typname.clone())
                .unwrap_or_default();
            let base = self.unwrap_domain(domain);
            let collation = self.pg_type.get(&base).and_then(|t| t.typcollation);
            let mut checks: Vec<&crate::ddl::types::DomainConstraint> = self
                .domain_constraints
                .get(&domain)
                .into_iter()
                .flatten()
                .filter(|c| c.kind == crate::ddl::types::DomainConstraintKind::Check)
                .collect();
            checks.sort_by(|a, b| a.name.cmp(&b.name));
            for check in checks {
                let mut r = check.allowed.clone().unwrap_or_default();
                r.finite |= check.finite;
                let parts = match (&check.expr, &check.trusted) {
                    (
                        Some(crate::ddl::tables::check_inherit::StoredExpr::Written(expr)),
                        Some(trust),
                    ) => crate::nonnull::checks::check_conjuncts((**expr).as_ref())
                        .into_iter()
                        .map(|n| CheckPart {
                            key: node_key("domain:", n),
                            columns: vec![column.to_owned()],
                            // domainAddCheckConstraint reads `VALUE` as a
                            // column of this name.
                            reading: crate::nonnull::checks::read_conjunct(
                                n, "value", base, collation, self, trust,
                            ),
                        })
                        .collect(),
                    _ => vec![CheckPart {
                        key: format!("unread-domain:{name}.{}", check.name),
                        columns: vec![column.to_owned()],
                        reading: CheckReading::Other,
                    }],
                };
                out.push(WriteConstraint {
                    source: format!("CHECK constraint \"{}\" of domain \"{name}\"", check.name),
                    columns: vec![column.to_owned()],
                    allowed: (r != Refinement::NONE).then_some(r),
                    parts,
                    not_valid: false,
                });
            }
        }
        out
    }

    /// What the CHECK conjunct `key` — the [`CheckPart::key`] of a part of
    /// one of [`Self::column_write_constraints`]`(relid, column)` —
    /// evaluates to over the row an INSERT (`update` false) or an UPDATE
    /// writes into relation `relid` when `row` holds the values the
    /// statement writes into its columns (by column name; a column it
    /// doesn't give is not known). Every column the conjunct reads must be
    /// written a constant, or one of a few (a `CASE` of constants, NULL
    /// when it may be NULL): the conjunct is evaluated over each
    /// combination of them, with PostgreSQL's built-in comparisons of
    /// integers, booleans and strings, `length` and `upper` / `lower` of
    /// text, `AND` / `OR` / `NOT` and the NULL tests (see
    /// [`crate::nonnull::subst::Eval`]); a CHECK passes a row it is NULL
    /// for. `Unknown` when a column it reads is not written a constant,
    /// when something can't be evaluated, when the relation isn't a table
    /// or a DO INSTEAD rule or a BEFORE ROW trigger may change the row
    /// before it is checked.
    pub fn check_part_outcome(
        &self,
        relid: crate::oid::PgClassOid,
        column: &str,
        key: &str,
        row: &std::collections::HashMap<String, &WrittenValue>,
        update: bool,
    ) -> PartOutcome {
        use crate::ddl::tables::check_inherit::StoredExpr;
        use crate::resolve::DmlEvent;
        let Some(class) = self.pg_class.get(&relid) else {
            return PartOutcome::Unknown;
        };
        let event = if update {
            DmlEvent::Update
        } else {
            DmlEvent::Insert
        };
        if !matches!(
            class.relkind,
            crate::pg_catalog::RelKind::Table | crate::pg_catalog::RelKind::Partitioned
        ) || crate::resolve::returning_rewritten(self, relid, &[event])
            || crate::resolve::before_row_trigger_rewrites(self, relid, event)
        {
            return PartOutcome::Unknown;
        }
        let attrs = self.attributes_of(relid);
        let Some(attr) = attrs.iter().find(|a| a.attname == column && a.attnum > 0) else {
            return PartOutcome::Unknown;
        };
        let is_column = |name: &str| attrs.iter().any(|a| a.attnum > 0 && a.attname == name);
        for con in self.pg_constraint.values().filter(|c| {
            c.conrelid == relid
                && c.contype == crate::pg_catalog::ConType::Check
                && c.conenforced
                && c.conkey.contains(&attr.attnum)
        }) {
            let Some(def) = self.check_defs.get(&con.oid) else {
                continue;
            };
            let (StoredExpr::Written(expr), Some(trust)) = (&def.expr, &def.trusted) else {
                continue;
            };
            for n in crate::nonnull::checks::check_conjuncts((**expr).as_ref()) {
                if node_key("relation:", n) == key {
                    let read: Vec<(String, String)> = read_columns(n, is_column)
                        .into_iter()
                        .map(|c| (c.clone(), c))
                        .collect();
                    return self.evaluate_conjunct(relid, n, trust, &read, row);
                }
            }
        }
        for domain in self.domain_chain(attr.atttypid) {
            for check in self.domain_constraints.get(&domain).into_iter().flatten() {
                let (Some(StoredExpr::Written(expr)), Some(trust)) = (&check.expr, &check.trusted)
                else {
                    continue;
                };
                for n in crate::nonnull::checks::check_conjuncts((**expr).as_ref()) {
                    if node_key("domain:", n) == key {
                        // domainAddCheckConstraint reads `VALUE` as a
                        // column of this name.
                        let read = [("value".to_owned(), column.to_owned())];
                        return self.evaluate_conjunct(relid, n, trust, &read, row);
                    }
                }
            }
        }
        PartOutcome::Unknown
    }

    /// Whether the CHECK conjunct `key` of relation `relid` — the
    /// [`CheckPart::key`] of a relation constraint's part — lets through
    /// every row an enforced CHECK constraint of relation `old_relid` of
    /// `old` lets through (`NOT VALID` ones included: they bind the rows
    /// written), so it restricts nothing more than that schema did: a
    /// CHECK passes a row it is NULL for, so this is PostgreSQL's weak
    /// implication (`predicate_implied_by(…, weak = true)`).
    ///
    /// Proven: an old conjunct equal to it; a constant `true` or NULL; and
    /// a `CASE` over one column whose `WHEN`s are constants it equals (a
    /// simple `CASE col WHEN 'a' THEN …`, or a searched one of `WHEN col =
    /// 'a'`), compared value by value with an old `CASE` over the same
    /// column: for each value either names, and for the values neither
    /// does (the `ELSE`s, which NULL reaches too), the old branch it takes
    /// must imply the new one — the same, recursively, or an old `false`.
    /// Adding a branch whose body is `true` to a `CASE … ELSE false END`,
    /// or replacing a body by `true`, widens it. At any depth, an `AND`
    /// that lost operands, an `OR` that gained some, or either reordered is
    /// implied too. The comparisons must be
    /// built-in ones of a type whose equal values print the same (no
    /// nondeterministic collation), so two different constants are two
    /// different values.
    pub fn check_part_implied_by(
        &self,
        relid: crate::oid::PgClassOid,
        key: &str,
        old: &PgCatalog,
        old_relid: crate::oid::PgClassOid,
    ) -> bool {
        let Some((new_conjunct, new_trust)) =
            self.relation_conjunct(relid, |n| node_key("relation:", n) == key)
        else {
            return false;
        };
        let new = Side {
            catalog: self,
            relid,
            trust: new_trust,
        };
        old.pg_constraint
            .values()
            .filter(|c| {
                c.conrelid == old_relid
                    && c.contype == crate::pg_catalog::ConType::Check
                    && c.conenforced
            })
            .filter_map(|c| old.check_defs.get(&c.oid))
            .any(|def| {
                let (crate::ddl::tables::check_inherit::StoredExpr::Written(expr), Some(trust)) =
                    (&def.expr, &def.trusted)
                else {
                    return false;
                };
                let old_side = Side {
                    catalog: old,
                    relid: old_relid,
                    trust,
                };
                crate::nonnull::checks::check_conjuncts((**expr).as_ref())
                    .into_iter()
                    .any(|o| weakly_implies((o, &old_side), (new_conjunct, &new)))
            })
    }

    /// The first top-level conjunct of an enforced CHECK constraint of
    /// `relid` that `matches`, with the constraint's trusted nodes.
    fn relation_conjunct(
        &self,
        relid: crate::oid::PgClassOid,
        matches: impl Fn(Option<&ast::Node>) -> bool,
    ) -> Option<(Option<&ast::Node>, &crate::nonnull::TrustedNodes)> {
        use crate::ddl::tables::check_inherit::StoredExpr;
        self.pg_constraint
            .values()
            .filter(|c| {
                c.conrelid == relid
                    && c.contype == crate::pg_catalog::ConType::Check
                    && c.conenforced
            })
            .filter_map(|c| self.check_defs.get(&c.oid))
            .find_map(|def| {
                let (StoredExpr::Written(expr), Some(trust)) = (&def.expr, &def.trusted) else {
                    return None;
                };
                crate::nonnull::checks::check_conjuncts((**expr).as_ref())
                    .into_iter()
                    .find(|n| matches(*n))
                    .map(|n| (n, trust))
            })
    }

    /// [`Self::check_part_outcome`] of conjunct `n`, whose column
    /// references `read` maps to the columns of `relid` they read.
    fn evaluate_conjunct(
        &self,
        relid: crate::oid::PgClassOid,
        n: Option<&ast::Node>,
        trust: &crate::nonnull::TrustedNodes,
        read: &[(String, String)],
        row: &std::collections::HashMap<String, &WrittenValue>,
    ) -> PartOutcome {
        use crate::nonnull::subst::{Eval, Val};
        // The constants each column read may hold.
        let mut choices: Vec<(&str, Vec<Val>)> = Vec::new();
        for (name, column) in read {
            let Some(attr) = self
                .attributes_of(relid)
                .iter()
                .find(|a| a.attnum > 0 && &a.attname == column)
            else {
                return PartOutcome::Unknown;
            };
            let Some(vals) = row
                .get(column)
                .and_then(|v| self.written_constants(v, attr))
            else {
                return PartOutcome::Unknown;
            };
            choices.push((name, vals));
        }
        if choices.is_empty()
            || choices
                .iter()
                .try_fold(1usize, |n, (_, vals)| n.checked_mul(vals.len()))
                .is_none_or(|n| n > MAX_COMBINATIONS)
        {
            return PartOutcome::Unknown;
        }
        let (mut passes, mut fails) = (0usize, 0usize);
        let mut total = 0usize;
        let mut index = vec![0usize; choices.len()];
        loop {
            let column = |node: &ast::Node| -> Val {
                let ast::Node::ColumnRef(c) = node else {
                    return Val::Unknown;
                };
                let names = crate::expr::extract_string_fields(&c.fields);
                let Some(name) = names.last() else {
                    return Val::Unknown;
                };
                choices
                    .iter()
                    .zip(&index)
                    .find(|((n, _), _)| n == name)
                    .map_or(Val::Unknown, |((_, vals), &i)| vals[i].clone())
            };
            match (Eval {
                column: &column,
                trust,
                snapshot: self,
            })
            .eval(n)
            {
                Val::Bool(true) | Val::Null => passes += 1,
                Val::Bool(false) => fails += 1,
                _ => return PartOutcome::Unknown,
            }
            total += 1;
            // The next combination.
            let mut k = 0;
            while k < index.len() {
                index[k] += 1;
                if index[k] < choices[k].1.len() {
                    break;
                }
                index[k] = 0;
                k += 1;
            }
            if k == index.len() {
                break;
            }
        }
        if passes == total {
            PartOutcome::Passes
        } else if fails == total {
            PartOutcome::Fails
        } else {
            PartOutcome::Unknown
        }
    }

    /// The constants `value` may be once stored into column `attr`, as
    /// [`crate::nonnull::subst::Val`]s: each of the few values it is known
    /// to be, of a type whose equal values print the same (an integer, a
    /// boolean, a string under a deterministic collation — not a padded
    /// `char(n)`), and NULL when it may be NULL. `None` when that isn't
    /// known: a parameter, `DEFAULT`, a value of unknown contents, one the
    /// column's type modifier may change.
    fn written_constants(
        &self,
        value: &WrittenValue,
        attr: &crate::pg_catalog::PgAttribute,
    ) -> Option<Vec<crate::nonnull::subst::Val>> {
        use crate::nonnull::subst::Val;
        let WrittenValue::Expression {
            type_oid,
            nullable,
            refinement,
            ..
        } = value
        else {
            return None;
        };
        // A bare NULL: an untyped value with no known contents.
        if *type_oid == crate::pg_catalog::oid::UNKNOWN && refinement.values.is_none() {
            return nullable.then(|| vec![Val::Null]);
        }
        let kind = Exact::of(attr.atttypid, attr.attcollation, self)?;
        let converted = refinement.converted(*type_oid, attr.atttypid, self);
        let values: Vec<String> = match (&converted.values, converted.range) {
            (Some(vs), _) => vs.iter().cloned().collect(),
            (
                None,
                Some(IntRange {
                    lo: Some(lo),
                    hi: Some(hi),
                }),
            ) if lo == hi => vec![lo.to_string()],
            _ => return None,
        };
        let typmod = self.effective_typmod(attr.atttypid, attr.atttypmod);
        let mut out: Vec<Val> = values
            .iter()
            .map(|v| match kind {
                Exact::Int => v.parse().ok().map(Val::Int),
                Exact::Bool => match v.as_str() {
                    "true" => Some(Val::Bool(true)),
                    "false" => Some(Val::Bool(false)),
                    _ => None,
                },
                // A `varchar(n)` keeps a string of at most n characters as
                // is.
                Exact::Text => typmod
                    .is_none_or(|m| {
                        i64::try_from(v.chars().count()).is_ok_and(|n| n <= i64::from(m) - 4)
                    })
                    .then(|| Val::Text(v.clone())),
            })
            .collect::<Option<_>>()?;
        if *nullable {
            out.push(Val::Null);
        }
        Some(out)
    }

    /// For the lint: what is known of a value of type `from`, known `r`
    /// about, once converted to type `to` — a column's, by the assignment
    /// that stores it (see [`Refinement::converted`]: an untyped literal
    /// read by the type's input function, an integer's range as `numeric`
    /// bounds, a `date`'s bounds as `timestamp` ones). A type modifier
    /// the conversion applies is not accounted for.
    pub fn lint_converted_refinement(
        &self,
        r: &Refinement,
        from: PgTypeOid,
        to: PgTypeOid,
    ) -> Refinement {
        r.converted(from, to, self)
    }

    /// For the lint: whether column `column` of relation `relid` has a
    /// default an INSERT that leaves it out (or `SET column = DEFAULT`)
    /// stores: a DEFAULT of its own, an identity or generation expression,
    /// or — none of those — its domain's DEFAULT (`build_column_default`
    /// falls back on the type's).
    pub fn lint_column_has_default(&self, relid: crate::oid::PgClassOid, column: &str) -> bool {
        self.attributes_of(relid)
            .iter()
            .find(|a| a.attnum > 0 && a.attname == column)
            .is_some_and(|a| {
                a.atthasdef
                    || a.attidentity.is_some()
                    || a.attgenerated.is_some()
                    || self.domain_has_default(a.atttypid)
            })
    }

    /// For the lint: the value an INSERT (or a COPY) that leaves out column
    /// `column` of relation `relid` stores — its DEFAULT expression, or
    /// NULL without one; `None` when not known (an identity, serial or
    /// generated column, a domain's default, a default that doesn't infer).
    pub fn lint_column_default_value(
        &self,
        relid: crate::oid::PgClassOid,
        column: &str,
    ) -> Option<WrittenValue> {
        let attr = self
            .attributes_of(relid)
            .iter()
            .find(|a| a.attnum > 0 && a.attname == column)?;
        crate::resolve::default_written_value(self, relid, attr)
    }

    /// What `r` allows column `column` of relation `relid`, in words (`one
    /// of 'a', 'b'`, `at least 0`, `finite`); `None` when it says nothing
    /// the words can tell.
    pub fn describe_column_values(
        &self,
        relid: crate::oid::PgClassOid,
        column: &str,
        r: &Refinement,
    ) -> Option<String> {
        let attr = self
            .attributes_of(relid)
            .iter()
            .find(|a| a.attname == column)?;
        let kind = match Exact::of(attr.atttypid, attr.attcollation, self) {
            Some(e) => Some(Kind::Exact(e)),
            None => OrdKind::of(attr.atttypid, self).map(Kind::Ord),
        };
        let text = kind
            .and_then(|kind| describe(r, kind, Cmp::Eq))
            .filter(|text| text != "unbounded" && !text.ends_with("the bounds known for it"))
            .map(|text| text.strip_prefix("always ").unwrap_or(&text).to_owned());
        match text {
            Some(text) if r.finite => Some(format!("finite and {text}")),
            Some(text) => Some(text),
            None => r.finite.then(|| "finite".to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datetimes_in_words() {
        // Day counts and microseconds as PostgreSQL 19 computes them
        // (`'2000-01-01'::date - '0044-03-15 BC'::date` is 746117).
        assert_eq!(datetime_words(0, OrdKind::Date).unwrap(), "2000-01-01");
        assert_eq!(datetime_words(8036, OrdKind::Date).unwrap(), "2022-01-01");
        assert_eq!(
            datetime_words(-746_117, OrdKind::Date).unwrap(),
            "0044-03-15 BC"
        );
        assert_eq!(
            datetime_words(-500_000, OrdKind::Timestamp).unwrap(),
            "1999-12-31 23:59:59.5"
        );
        assert_eq!(
            datetime_words(3_600_000_000, OrdKind::TimestampTz).unwrap(),
            "2000-01-01 01:00:00+00"
        );
        assert_eq!(datetime_words(1, OrdKind::Numeric), None);
    }
}
