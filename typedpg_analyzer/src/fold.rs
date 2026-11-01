//! The integer and `numeric` constants PostgreSQL's planner folds.
//!
//! Parse analysis leaves a conversion or an operator over constants as a
//! function call (`40000::int2` is `int2(40000)`, `2147483647 + 1` is
//! `int4pl(2147483647, 1)`); the planner's `eval_const_expressions` runs
//! every immutable one whose arguments are constants while it plans the
//! statement. One that fails — out of range, a division by zero — fails
//! every execution, before any row is read, so `PREPARE` succeeds and
//! each `EXECUTE` raises the error. This module computes what that folding
//! makes of an expression built from integer and `numeric` literals, the
//! built-in arithmetic operators and casts between those types, so the
//! analyzer can report the error at compile time.
//!
//! Folding only ever answers for what it models: anything else (a column
//! the planner doesn't replace by a pulled-up subquery's output, a
//! parameter, a function call, an operator a user may have redefined) is
//! `None`, and the expression is left alone.

use typedpg_pg_query::protobuf::{self, a_const, node};

use crate::error::{AnalyzeError, RawError, SourceSpan};
use crate::oid::PgTypeOid;
use crate::pg_catalog::{PgCatalog, oid};

/// One of PG's integer types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum IntType {
    Int2,
    Int4,
    Int8,
}

impl IntType {
    /// The integer type `t` is (not through a domain).
    pub(crate) fn of(t: PgTypeOid) -> Option<IntType> {
        match t {
            oid::INT2 => Some(IntType::Int2),
            oid::INT4 => Some(IntType::Int4),
            oid::INT8 => Some(IntType::Int8),
            _ => None,
        }
    }

    fn contains(self, v: i128) -> bool {
        match self {
            IntType::Int2 => i16::try_from(v).is_ok(),
            IntType::Int4 => i32::try_from(v).is_ok(),
            IntType::Int8 => i64::try_from(v).is_ok(),
        }
    }

    /// Its name in PG's messages (`format_type`).
    fn name(self) -> &'static str {
        match self {
            IntType::Int2 => "smallint",
            IntType::Int4 => "integer",
            IntType::Int8 => "bigint",
        }
    }

    /// `v` as this type: the value, or the error its conversion raises.
    fn checked(self, v: i128) -> Result<Value, FoldError> {
        if self.contains(v) {
            Ok(Value::Int(v, self))
        } else {
            Err(FoldError::OutOfRange(self))
        }
    }
}

/// A folded constant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
    /// An integer, of its type.
    Int(i128, IntType),
    /// A `numeric`, as text `numeric_in` accepts (`1.5`, `1e400`, `NaN`,
    /// `-Infinity`).
    Numeric(String),
}

/// The error folding raises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FoldError {
    /// `smallint out of range` and its kin.
    OutOfRange(IntType),
    /// `numeric field overflow`, with its detail.
    NumericOverflow(String),
    DivisionByZero,
    /// `cannot convert NaN to integer` / `cannot convert infinity to …`.
    Special {
        value: &'static str,
        to: IntType,
    },
}

impl FoldError {
    /// The error, pointing at `span`.
    pub(crate) fn raw(&self, span: Option<SourceSpan>) -> RawError {
        match self {
            FoldError::OutOfRange(t) => crate::pgmsg::integer_out_of_range(t.name(), span),
            FoldError::NumericOverflow(msg) => {
                crate::pgmsg::numeric_field_overflow(msg.clone(), span)
            }
            FoldError::DivisionByZero => crate::pgmsg::division_by_zero(span),
            FoldError::Special { value, to } => {
                crate::pgmsg::cannot_convert_special_numeric(value, to.name(), span)
            }
        }
    }

    /// The error, pointing at `node`.
    pub(crate) fn at(&self, node: &protobuf::Node) -> AnalyzeError {
        self.raw(crate::error::expr_span(node)).finalize_implicit()
    }
}

/// What folding makes of an expression: `None` when it isn't a constant
/// this module models.
pub(crate) type Folded = Option<Result<Value, FoldError>>;

/// The error the planner raises folding `node`, if it certainly does.
pub(crate) fn error(node: &protobuf::Node, snapshot: &PgCatalog) -> Option<FoldError> {
    fold(node, snapshot)?.err()
}

/// What the planner substitutes for a column reference before folding
/// (the output expression of a subquery it pulls up), if anything.
pub(crate) type Columns<'a> =
    dyn Fn(&protobuf::ColumnRef) -> Option<std::sync::Arc<protobuf::Node>> + 'a;

/// [`error`] for `node` in a scope whose columns `columns` substitutes:
/// `x::int2` over `(SELECT 40000 AS x) s` is `40000::int2` once `s` is
/// pulled up (`pullup_replace_vars` precedes `eval_const_expressions`).
pub(crate) fn error_in(
    node: &protobuf::Node,
    snapshot: &PgCatalog,
    columns: &Columns<'_>,
) -> Option<FoldError> {
    fold_with(node, snapshot, columns)?.err()
}

/// The error the planner raises folding `node` once converted to `target`
/// (of typmod `typmod`) — an assignment to a column of that type, or a
/// LIMIT's conversion to `bigint` — if it certainly does.
pub(crate) fn conversion_error(
    node: &protobuf::Node,
    target: PgTypeOid,
    typmod: Option<i32>,
    snapshot: &PgCatalog,
) -> Option<FoldError> {
    let value = match node.node.as_ref()? {
        // An untyped literal is read as the target type at parse time
        // (that reading's errors are the literal's), then converted.
        node::Node::AConst(c) if matches!(c.val, Some(a_const::Val::Sval(_))) => {
            let Some(a_const::Val::Sval(s)) = &c.val else {
                return None;
            };
            literal_as(&s.sval, snapshot.unwrap_domain(target))?
        }
        _ => fold(node, snapshot)?.ok()?,
    };
    convert(&value, target, typmod, snapshot)?.err()
}

/// Fold `node`.
pub(crate) fn fold(node: &protobuf::Node, snapshot: &PgCatalog) -> Folded {
    fold_with(node, snapshot, &|_| None)
}

fn fold_with(node: &protobuf::Node, snapshot: &PgCatalog, columns: &Columns<'_>) -> Folded {
    crate::stack::step(|| fold_here(node, snapshot, columns))
}

fn fold_here(node: &protobuf::Node, snapshot: &PgCatalog, columns: &Columns<'_>) -> Folded {
    match node.node.as_ref()? {
        // The substituted expression is the subquery's: its own columns
        // are of the subquery's scope (and already substituted there).
        node::Node::ColumnRef(cr) => fold(&*columns(cr)?, snapshot),
        node::Node::AConst(c) if !c.isnull => match c.val.as_ref()? {
            a_const::Val::Ival(i) => Some(Ok(Value::Int(i.ival.into(), IntType::Int4))),
            // `make_const`: an integer too large for `int4` is `int8`, then
            // `numeric`; anything else written with a point or an exponent
            // is `numeric`.
            a_const::Val::Fval(f) => match crate::literal_input::parse_pg_integer(&f.fval) {
                Some(v) if IntType::Int4.contains(v) => Some(Ok(Value::Int(v, IntType::Int4))),
                Some(v) if IntType::Int8.contains(v) => Some(Ok(Value::Int(v, IntType::Int8))),
                _ => crate::literal_input::validate_numeric(&f.fval)
                    .ok()
                    .map(|()| Ok(Value::Numeric(f.fval.clone()))),
            },
            _ => None,
        },
        node::Node::TypeCast(tc) => {
            let tn = tc.type_name.as_ref()?;
            if !tn.array_bounds.is_empty() {
                return None;
            }
            let target = crate::ddl::util::resolve_type_name(tn, snapshot)?;
            let typmod = if tn.typmods.is_empty() {
                None
            } else {
                crate::typmod::encode(snapshot, target, tn).ok()?
            };
            let arg = tc.arg.as_deref()?;
            let value = match arg.node.as_ref()? {
                node::Node::AConst(c) if matches!(c.val, Some(a_const::Val::Sval(_))) => {
                    let Some(a_const::Val::Sval(s)) = &c.val else {
                        return None;
                    };
                    literal_as(&s.sval, snapshot.unwrap_domain(target))?
                }
                _ => match fold_with(arg, snapshot, columns)? {
                    Ok(v) => v,
                    Err(e) => return Some(Err(e)),
                },
            };
            convert(&value, target, typmod, snapshot)
        }
        node::Node::AExpr(e) => fold_operator(e, snapshot, columns),
        _ => None,
    }
}

/// An untyped literal read as type `base` (`int4in`, `numeric_in`, …);
/// `None` when it doesn't read (an error parse analysis reports) or is
/// of a type this module doesn't model.
fn literal_as(text: &str, base: PgTypeOid) -> Option<Value> {
    if let Some(t) = IntType::of(base) {
        let v = crate::literal_input::parse_pg_integer(text)?;
        return t.contains(v).then_some(Value::Int(v, t));
    }
    (base == oid::NUMERIC && crate::literal_input::validate_numeric(text).is_ok())
        .then(|| Value::Numeric(text.to_owned()))
}

/// `NaN`, `[+-]inf[inity]` or a finite value, of a `numeric` text.
#[derive(PartialEq)]
enum NumericKind {
    NaN,
    Infinity,
    Finite,
}

fn numeric_kind(raw: &str) -> NumericKind {
    let s = raw.trim_matches(|c: char| c.is_ascii_whitespace());
    let s = s.strip_prefix(['+', '-']).unwrap_or(s);
    match s.to_ascii_lowercase().as_str() {
        "nan" => NumericKind::NaN,
        "inf" | "infinity" => NumericKind::Infinity,
        _ => NumericKind::Finite,
    }
}

/// A finite `numeric` text as an exact decimal, when it is simple enough.
fn numeric_decimal(raw: &str) -> Option<crate::decimal::Decimal> {
    crate::decimal::Decimal::parse(raw.trim_matches(|c: char| c.is_ascii_whitespace()))
}

/// `value` converted to `target` (a domain converts to its base type,
/// with its typmod): an integer type's range check (`int2(int4)`,
/// `numeric_int4`'s rounding), `numeric(p, s)`'s `apply_typmod`.
pub(crate) fn convert(
    value: &Value,
    target: PgTypeOid,
    typmod: Option<i32>,
    snapshot: &PgCatalog,
) -> Folded {
    let base = snapshot.unwrap_domain(target);
    let typmod = if base == target {
        typmod
    } else {
        snapshot.effective_typmod(target, None)
    };
    if let Some(to) = IntType::of(base) {
        return match value {
            Value::Int(v, _) => Some(to.checked(*v)),
            Value::Numeric(raw) => numeric_to_int(raw, to),
        };
    }
    if base != oid::NUMERIC {
        return None;
    }
    let raw = match value {
        Value::Int(v, _) => v.to_string(),
        Value::Numeric(raw) => raw.clone(),
    };
    let crate::typmod::DecodedTypmod::Numeric { precision, scale } =
        crate::typmod::decode(snapshot, base, typmod)
    else {
        return Some(Ok(Value::Numeric(raw)));
    };
    if let Some(msg) = crate::typmod::numeric_overflow(&raw, precision, scale) {
        return Some(Err(FoldError::NumericOverflow(msg)));
    }
    // The value is rounded to the scale: only an integer (or NaN) at a
    // scale ≥ 0 is known to stay what it was.
    let unchanged = scale >= 0
        && (numeric_kind(&raw) == NumericKind::NaN
            || crate::literal_input::parse_pg_integer(&raw).is_some());
    unchanged.then_some(Ok(Value::Numeric(raw)))
}

/// `numeric_int2` / `numeric_int4` / `numeric_int8`: NaN and the
/// infinities don't convert; a finite value is rounded (halves away from
/// zero), then must fit.
fn numeric_to_int(raw: &str, to: IntType) -> Folded {
    match numeric_kind(raw) {
        NumericKind::NaN => return Some(Err(FoldError::Special { value: "NaN", to })),
        NumericKind::Infinity => {
            return Some(Err(FoldError::Special {
                value: "infinity",
                to,
            }));
        }
        NumericKind::Finite => {}
    }
    match numeric_decimal(raw).and_then(|d| d.round()?.to_integer()) {
        Some(v) => Some(to.checked(v)),
        // Too many digits to read exactly: at least 10^19 once rounded is
        // beyond every integer type.
        None => crate::typmod::numeric_overflow(raw, 19, 0).map(|_| Err(FoldError::OutOfRange(to))),
    }
}

/// An operand of an operator: a folded constant, or an untyped literal
/// (read as the other operand's type).
enum Operand {
    Value(Value),
    Untyped(String),
}

fn operand(
    node: &protobuf::Node,
    snapshot: &PgCatalog,
    columns: &Columns<'_>,
) -> Option<Result<Operand, FoldError>> {
    if let Some(node::Node::AConst(c)) = node.node.as_ref()
        && !c.isnull
        && let Some(a_const::Val::Sval(s)) = &c.val
    {
        return Some(Ok(Operand::Untyped(s.sval.clone())));
    }
    Some(fold_with(node, snapshot, columns)?.map(Operand::Value))
}

/// The built-in arithmetic operators, over integer and `numeric`
/// constants: `int4pl`, `int48mi`, `int2um`, `numeric_div`, …
fn fold_operator(e: &protobuf::AExpr, snapshot: &PgCatalog, columns: &Columns<'_>) -> Folded {
    if protobuf::AExprKind::try_from(e.kind) != Ok(protobuf::AExprKind::AexprOp) {
        return None;
    }
    let names = crate::expr::extract_string_fields(&e.name);
    let op = match names.as_slice() {
        [op] => op.as_str(),
        [schema, op] if schema == "pg_catalog" => op.as_str(),
        _ => return None,
    };
    if !matches!(op, "+" | "-" | "*" | "/" | "%") {
        return None;
    }
    let right = e.rexpr.as_deref()?;
    let Some(left) = e.lexpr.as_deref() else {
        // Prefix `-` / `+` (`int4um` / `int4up`).
        return match fold_with(right, snapshot, columns)? {
            Err(err) => Some(Err(err)),
            Ok(_) if shadowed(op, snapshot) => None,
            Ok(Value::Int(v, t)) => match op {
                "-" => Some(t.checked(-v)),
                "+" => Some(Ok(Value::Int(v, t))),
                _ => None,
            },
            Ok(Value::Numeric(_)) => None,
        };
    };
    // The arguments are folded first, left to right.
    let l = match operand(left, snapshot, columns) {
        Some(Err(err)) => return Some(Err(err)),
        l => l,
    };
    let r = match operand(right, snapshot, columns) {
        Some(Err(err)) => return Some(Err(err)),
        r => r,
    };
    let (l, r) = (l?.ok()?, r?.ok()?);
    // An untyped literal takes the other side's type.
    let (l, r) = match (l, r) {
        (Operand::Value(l), Operand::Value(r)) => (l, r),
        (Operand::Untyped(s), Operand::Value(v)) => (typed_like(&s, &v)?, v),
        (Operand::Value(v), Operand::Untyped(s)) => {
            let typed = typed_like(&s, &v)?;
            (v, typed)
        }
        (Operand::Untyped(_), Operand::Untyped(_)) => return None,
    };
    if shadowed(op, snapshot) {
        return None;
    }
    match (&l, &r) {
        (Value::Int(a, ta), Value::Int(b, tb)) => {
            // `int24pl` & co.: the wider type.
            let t = (*ta).max(*tb);
            let (a, b) = (*a, *b);
            let v = match op {
                "+" => a + b,
                "-" => a - b,
                "*" => a * b,
                "/" | "%" if b == 0 => return Some(Err(FoldError::DivisionByZero)),
                "/" => a / b,
                // `int4mod`: `INT_MIN % -1` is 0, not an overflow.
                _ if b == -1 => 0,
                _ => a % b,
            };
            Some(t.checked(v))
        }
        // `numeric_div` / `numeric_mod` (an integer side is converted to
        // `numeric`): a zero divisor fails unless the dividend is NaN.
        _ if matches!(op, "/" | "%") => {
            let dividend_nan =
                matches!(&l, Value::Numeric(raw) if numeric_kind(raw) == NumericKind::NaN);
            let divisor_zero = match &r {
                Value::Int(b, _) => *b == 0,
                Value::Numeric(raw) => {
                    numeric_kind(raw) == NumericKind::Finite && numeric_decimal(raw)?.is_zero()
                }
            };
            (divisor_zero && !dividend_nan).then_some(Err(FoldError::DivisionByZero))
        }
        _ => None,
    }
}

/// Untyped literal `text` read as the type of `other`, the operator's
/// other operand (`oper_select_candidate` picks the operator taking the
/// known type on both sides).
fn typed_like(text: &str, other: &Value) -> Option<Value> {
    match other {
        Value::Int(_, t) => literal_as(text, int_oid(*t)),
        Value::Numeric(_) => literal_as(text, oid::NUMERIC),
    }
}

fn int_oid(t: IntType) -> PgTypeOid {
    match t {
        IntType::Int2 => oid::INT2,
        IntType::Int4 => oid::INT4,
        IntType::Int8 => oid::INT8,
    }
}

/// Whether an operator named `op` exists outside `pg_catalog`: a search
/// path listing its schema before `pg_catalog` would resolve to it, so
/// the built-in arithmetic isn't certain.
fn shadowed(op: &str, snapshot: &PgCatalog) -> bool {
    snapshot
        .pg_operator
        .values()
        .any(|o| o.oprname == op && snapshot.namespace_name(o.oprnamespace) != Some("pg_catalog"))
}
