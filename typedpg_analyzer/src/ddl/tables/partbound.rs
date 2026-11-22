//! Partition bounds (`FOR VALUES ...` / `DEFAULT`): PG validates a new
//! partition's bound against the parent's strategy and key types
//! (`transformPartitionBound`) and against the sibling partitions
//! (`check_new_partition_bound`: overlaps, one default partition, hash
//! moduli dividing each other).
//!
//! A bound is a constant expression, evaluated (transformPartitionBoundValue
//! → evaluate_expr) and compared with the key's btree comparison. The
//! analyzer compares the values it can compute and order exactly as that
//! comparison does (see [`normalize`]); any other value is [`Datum::Opaque`]
//! and never overlaps, nor bounds an empty range, as far as it can tell.

use super::*;
use typedpg_pg_query::ast::{self, PartitionBoundSpec};

/// A partitioned table's strategy and key (`pg_partitioned_table`).
#[derive(Clone, Debug)]
pub(crate) struct PartSpec {
    strategy: Strategy,
    keys: Vec<PartKey>,
}

impl PartSpec {
    /// RENAME COLUMN `old` TO `new` of the partitioned table.
    pub(crate) fn rename_key_column(&mut self, old: &str, new: &str) {
        for k in &mut self.keys {
            if k.name.as_deref() == Some(old) {
                k.name = Some(new.to_owned());
            }
        }
    }
}

/// One partition key column or expression.
#[derive(Clone, Debug)]
struct PartKey {
    type_oid: PgTypeOid,
    /// The column name, or `None` for an expression.
    name: Option<String>,
    /// `partcollation`.
    collation: Option<crate::oid::PgCollationOid>,
    /// The key's collation orders strings by code point, so string values
    /// order without a server.
    code_point_order: bool,
    /// `partclass`: the operator class (its family's equality operator is
    /// the key's notion of equality). `None` when the key's type is
    /// unknown to the analyzer.
    opclass: Option<crate::oid::PgOpclassOid>,
    /// The operator class is a built-in one: the key compares as its
    /// type's own comparison does (a user's may order values any way).
    builtin_order: bool,
    /// The attnums of the columns the key is strict in: the key column
    /// itself, or those a key expression is NULL for when they are.
    strict_attnums: Vec<i16>,
    /// A key column's type modifier (`varchar(2)`): a bound value is
    /// coerced to it (transformPartitionBoundValue).
    typmod: Option<i32>,
}

/// The collation a `COLLATE` clause names.
pub(crate) fn collation_clause(
    interp: &PgCatalog,
    names: &[Option<typedpg_pg_query::ast::Node>],
) -> Option<crate::oid::PgCollationOid> {
    let names: Vec<&str> = names
        .iter()
        .filter_map(|n| super::super::util::node_string(n.as_ref()))
        .collect();
    match names.as_slice() {
        [schema, name] => interp.resolve_collation(Some(schema), name),
        [name] => interp.resolve_collation(None, name),
        _ => None,
    }
    .map(|c| c.oid)
}

/// The partition key's collations (`partcollation`), in key order.
pub(crate) fn partition_key_collations(
    interp: &PgCatalog,
    relid: PgClassOid,
) -> Vec<Option<crate::oid::PgCollationOid>> {
    interp
        .partition_specs
        .get(&relid)
        .map(|s| s.keys.iter().map(|k| k.collation).collect())
        .unwrap_or_default()
}

/// The equality operator of each partition key column (DefineIndex's
/// `ptkey_eqop`): the COMPARE_EQ member of its operator class's
/// (`partclass`) family. `None` where the analyzer can't tell.
pub(crate) fn partition_key_eqops(
    interp: &PgCatalog,
    relid: PgClassOid,
) -> Vec<Option<crate::oid::PgOperatorOid>> {
    let Some(spec) = interp.partition_specs.get(&relid) else {
        return Vec::new();
    };
    spec.keys
        .iter()
        .map(|k| crate::ddl::indexes::index_eq_operator(interp, k.opclass?))
        .collect()
}

/// `text_pattern_ops` / `varchar_pattern_ops` compare strings byte by byte
/// (bttext_pattern_cmp), whatever the collation: code point order for
/// UTF-8. (`bpchar_pattern_ops` also ignores trailing blanks, which the
/// string comparison here doesn't model.)
fn compares_bytes(interp: &PgCatalog, opclass: Option<crate::oid::PgOpclassOid>) -> bool {
    opclass
        .and_then(|oid| crate::ddl::opclass::opclass_by_oid(interp, oid))
        .is_some_and(|c| {
            c.opcmethod == "btree"
                && matches!(
                    c.opcname.as_str(),
                    "text_pattern_ops" | "varchar_pattern_ops"
                )
                && interp.namespace_name(c.opcnamespace) == Some("pg_catalog")
        })
}

/// Whether `opclass` is one of `pg_catalog`'s: it compares as the type's
/// own comparison functions do.
fn builtin_opclass(interp: &PgCatalog, opclass: Option<crate::oid::PgOpclassOid>) -> bool {
    opclass
        .and_then(|oid| crate::ddl::opclass::opclass_by_oid(interp, oid))
        .is_some_and(|c| interp.namespace_name(c.opcnamespace) == Some("pg_catalog"))
}

/// The collations whose order is the strings' code point order: C / POSIX
/// (byte order, which for UTF-8 is code point order), `ucs_basic`, and the
/// builtin provider's `pg_c_utf8` / `pg_unicode_fast`.
pub(crate) fn orders_by_code_point(
    interp: &PgCatalog,
    collation: crate::oid::PgCollationOid,
) -> bool {
    interp.pg_collation.get(&collation).is_some_and(|c| {
        matches!(
            c.collname.as_str(),
            "C" | "POSIX" | "ucs_basic" | "pg_c_utf8" | "pg_unicode_fast"
        ) && interp.namespace_name(c.collnamespace) == Some("pg_catalog")
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Strategy {
    List,
    Range,
    Hash,
}

/// A bound value, as far as the analyzer can order it. The values of one
/// key are all of one kind.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Datum {
    /// A value whose order is that of an integer: an integer, a boolean,
    /// a date (days), a timestamp (microseconds), an interval
    /// (interval_cmp_value), an enum label (its sort order), a uuid.
    Int(i128),
    /// A `numeric` value.
    Numeric(NumericKey),
    /// A `float4` / `float8` value, as an unsigned key in float8_cmp's
    /// order: -0 is 0, NaN is above everything and equal to itself.
    Float(u64),
    /// A string key value under a collation that orders by code point.
    CodePoints(String),
    /// A string key value, compared by equality only (its order depends
    /// on the collation).
    Text(String),
    /// Anything else: equal to nothing, ordered against nothing.
    Opaque,
}

/// A `numeric` value in numeric_cmp's order: NaN above +Infinity, and
/// equal whatever the display scale (`1.0` = `1.00`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NumericKey {
    NegInfinity,
    /// `m × 10^-scale`, without trailing zeros; small enough for two to
    /// be aligned exactly (see [`NumericKey::parse`]).
    Finite {
        m: i128,
        scale: u32,
    },
    Infinity,
    NaN,
}

impl NumericKey {
    /// The value of a `numeric` constant's text (`numeric_in`), when the
    /// analyzer can compare it exactly: up to 18 significant digits and
    /// 18 decimal places.
    fn parse(raw: &str) -> Option<NumericKey> {
        let t = raw.trim_matches(|c: char| c.is_ascii_whitespace());
        let (negative, unsigned) = match t.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, t.strip_prefix('+').unwrap_or(t)),
        };
        match unsigned.to_ascii_lowercase().as_str() {
            "nan" if unsigned.len() == t.len() => return Some(NumericKey::NaN),
            "inf" | "infinity" if negative => return Some(NumericKey::NegInfinity),
            "inf" | "infinity" => return Some(NumericKey::Infinity),
            _ => {}
        }
        let (mantissa, exponent) = match unsigned.split_once(['e', 'E']) {
            Some((m, e)) => (m, e.parse::<i64>().ok()?),
            None => (unsigned, 0),
        };
        let (int_part, frac_part) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        if int_part.is_empty() && frac_part.is_empty() {
            return None;
        }
        let mut m: i128 = 0;
        for c in int_part.chars().chain(frac_part.chars()) {
            m = m
                .checked_mul(10)?
                .checked_add(i128::from(c.to_digit(10)?))?;
        }
        let mut scale = i64::try_from(frac_part.len()).ok()? - exponent;
        while scale < 0 {
            m = m.checked_mul(10)?;
            scale += 1;
        }
        while scale > 0 && m % 10 == 0 {
            m /= 10;
            scale -= 1;
        }
        if m.unsigned_abs() >= 10u128.pow(18) || scale > 18 {
            return None;
        }
        Some(NumericKey::Finite {
            m: if negative { -m } else { m },
            scale: u32::try_from(scale).ok()?,
        })
    }

    fn of(value: &crate::fold::Value) -> Option<NumericKey> {
        match value {
            crate::fold::Value::Int(i, _) => NumericKey::parse(&i.to_string()),
            crate::fold::Value::Numeric(raw) => NumericKey::parse(raw),
        }
    }

    fn rank(self) -> u8 {
        match self {
            NumericKey::NegInfinity => 0,
            NumericKey::Finite { .. } => 1,
            NumericKey::Infinity => 2,
            NumericKey::NaN => 3,
        }
    }
}

impl Ord for NumericKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (NumericKey::Finite { m: a, scale: sa }, NumericKey::Finite { m: b, scale: sb }) => {
                // Both below 10^18 at 18 places at most: aligned, below 10^36.
                let scale = (*sa).max(*sb);
                (a * 10i128.pow(scale - sa)).cmp(&(b * 10i128.pow(scale - sb)))
            }
            _ => self.rank().cmp(&other.rank()),
        }
    }
}

impl PartialOrd for NumericKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// `x` as an unsigned key in float8_cmp's order (see [`Datum::Float`]).
fn float_key(x: f64) -> u64 {
    if x.is_nan() {
        return u64::MAX;
    }
    let bits = if x == 0.0 { 0 } else { x.to_bits() };
    if bits >> 63 == 1 {
        !bits
    } else {
        bits | 1 << 63
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RangeDatum {
    MinValue,
    Value(Datum),
    MaxValue,
}

/// A partition's bound (`relpartbound`).
#[derive(Clone, Debug)]
pub(crate) enum Bound {
    Default,
    /// `None` is the NULL value.
    List(Vec<Option<Datum>>),
    Range {
        lower: Vec<RangeDatum>,
        upper: Vec<RangeDatum>,
    },
    Hash {
        modulus: i32,
        remainder: i32,
    },
}

/// Record a new partitioned table's strategy and key types
/// (ComputePartitionAttrs).
pub(super) fn record_partition_spec(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    spec: &typedpg_pg_query::ast::PartitionSpec,
) {
    use typedpg_pg_query::ast::PartitionStrategy as Ps;
    let strategy = match spec.strategy {
        Ps::List => Strategy::List,
        Ps::Hash => Strategy::Hash,
        _ => Strategy::Range,
    };
    let am = if strategy == Strategy::Hash {
        "hash"
    } else {
        "btree"
    };
    // ComputePartitionAttrs: the key's operator class, as written or the
    // type's default (its errors were reported when the key was checked).
    let opclass_of = |typ: PgTypeOid, pe: &typedpg_pg_query::ast::PartitionElem| {
        crate::ddl::opclass::resolve_index_opclass(interp, &pe.opclass, typ, am)
            .ok()
            .flatten()
    };
    let mut keys = Vec::new();
    for elem in &spec.part_params {
        let Some(ast::Node::PartitionElem(pe)) = elem.as_ref() else {
            continue;
        };
        // The key's collation: its COLLATE clause, else the column's.
        let explicit = (!pe.collation.is_empty()).then(|| collation_clause(interp, &pe.collation));
        if !pe.name.is_empty() {
            let attr = interp.attribute_by_name(relid, &pe.name);
            let collation = explicit.unwrap_or_else(|| attr.and_then(|a| a.attcollation));
            let type_oid = attr.map_or(crate::pg_catalog::oid::UNKNOWN, |a| a.atttypid);
            let opclass = opclass_of(type_oid, pe);
            keys.push(PartKey {
                type_oid,
                name: Some(pe.name.clone()),
                collation,
                code_point_order: collation.is_some_and(|c| orders_by_code_point(interp, c))
                    || compares_bytes(interp, opclass),
                builtin_order: builtin_opclass(interp, opclass),
                opclass,
                strict_attnums: attr.map(|a| a.attnum).into_iter().collect(),
                typmod: attr.and_then(|a| interp.effective_typmod(a.atttypid, a.atttypmod)),
            });
        } else if pe.expr.is_some() {
            let expr = &pe.expr;
            let (typ, expr_collation) =
                match crate::ddl::volatile::infer_over_relation(interp, relid, expr.as_ref(), None)
                {
                    // The expression's collation (exprCollation), else its
                    // type's default.
                    Some(Ok(t)) => (
                        t.type_oid,
                        t.collation
                            .or_else(|| interp.get_type(t.type_oid).and_then(|ty| ty.typcollation)),
                    ),
                    _ => (crate::pg_catalog::oid::UNKNOWN, None),
                };
            let collation = explicit.unwrap_or(expr_collation);
            let opclass = opclass_of(typ, pe);
            let builtin_order = builtin_opclass(interp, opclass);
            let strict_attnums =
                crate::ddl::volatile::strict_columns_over_relation(interp, relid, expr.as_ref())
                    .iter()
                    .filter_map(|c| interp.attribute_by_name(relid, c).map(|a| a.attnum))
                    .collect();
            keys.push(PartKey {
                type_oid: typ,
                name: None,
                collation,
                code_point_order: collation.is_some_and(|c| orders_by_code_point(interp, c))
                    || compares_bytes(interp, opclass),
                builtin_order,
                opclass,
                strict_attnums,
                typmod: None,
            });
        }
    }
    interp
        .partition_specs
        .insert(relid, PartSpec { strategy, keys });
}

/// Validate `spec` as the bound of partition `part` of `parent` and record
/// it (transformPartitionBound + check_new_partition_bound).
pub(super) fn add_partition_bound(
    interp: &mut PgCatalog,
    parent: PgClassOid,
    part: PgClassOid,
    spec: &PartitionBoundSpec,
) -> Result<(), DdlError> {
    let Some(pspec) = interp.partition_specs.get(&parent).cloned() else {
        return Ok(());
    };
    let bound = transform_bound(interp, &pspec, spec)?;
    check_new_bound(interp, parent, part, &bound)?;
    interp.partition_bounds.insert(part, bound);
    Ok(())
}

fn transform_bound(
    interp: &PgCatalog,
    pspec: &PartSpec,
    spec: &PartitionBoundSpec,
) -> Result<Bound, DdlError> {
    let invalid = |kind: &str| {
        DdlError::Parse(format!(
            "invalid bound specification for a {kind} partition"
        ))
    };
    if spec.is_default {
        if pspec.strategy == Strategy::Hash {
            return Err(DdlError::Parse(
                "a hash-partitioned table may not have a default partition".into(),
            ));
        }
        return Ok(Bound::Default);
    }
    match pspec.strategy {
        Strategy::Hash => {
            if spec.strategy != "h" {
                return Err(invalid("hash"));
            }
            if spec.modulus < 1 {
                return Err(DdlError::Parse(
                    "modulus for hash partition must be an integer value greater than zero".into(),
                ));
            }
            if spec.remainder < 0 {
                return Err(DdlError::Parse(
                    "remainder for hash partition must be an integer value greater than or \
                     equal to zero"
                        .into(),
                ));
            }
            if spec.remainder >= spec.modulus {
                return Err(DdlError::Parse(
                    "remainder for hash partition must be less than modulus".into(),
                ));
            }
            Ok(Bound::Hash {
                modulus: spec.modulus,
                remainder: spec.remainder,
            })
        }
        Strategy::List => {
            if spec.strategy != "l" {
                return Err(invalid("list"));
            }
            let key = pspec.keys.first().cloned().unwrap_or(PartKey {
                type_oid: crate::pg_catalog::oid::UNKNOWN,
                name: None,
                collation: None,
                code_point_order: false,
                builtin_order: false,
                opclass: None,
                strict_attnums: Vec::new(),
                typmod: None,
            });
            let mut values: Vec<Option<Datum>> = Vec::new();
            for v in &spec.listdatums {
                let d = bound_value(interp, v.as_ref(), &key)?;
                // Duplicates within one list are dropped.
                if d.as_ref().is_some_and(|d| *d == Datum::Opaque) || !values.contains(&d) {
                    values.push(d);
                }
            }
            Ok(Bound::List(values))
        }
        Strategy::Range => {
            if spec.strategy != "r" {
                return Err(invalid("range"));
            }
            if spec.lowerdatums.len() != pspec.keys.len() {
                return Err(DdlError::Parse(
                    "FROM must specify exactly one value per partitioning column".into(),
                ));
            }
            if spec.upperdatums.len() != pspec.keys.len() {
                return Err(DdlError::Parse(
                    "TO must specify exactly one value per partitioning column".into(),
                ));
            }
            let lower = range_datums(interp, &spec.lowerdatums, &pspec.keys)?;
            let upper = range_datums(interp, &spec.upperdatums, &pspec.keys)?;
            Ok(Bound::Range { lower, upper })
        }
    }
}

/// transformPartitionRangeBounds + validateInfiniteBounds.
fn range_datums(
    interp: &PgCatalog,
    values: &[Option<typedpg_pg_query::ast::Node>],
    keys: &[PartKey],
) -> Result<Vec<RangeDatum>, DdlError> {
    let mut out = Vec::new();
    for (v, key) in values.iter().zip(keys) {
        let infinite = match v.as_ref() {
            Some(ast::Node::ColumnRef(cr)) if cr.fields.len() == 1 => {
                match cr
                    .fields
                    .first()
                    .and_then(|n| super::super::util::node_string(n.as_ref()))
                {
                    Some(n) if n.eq_ignore_ascii_case("minvalue") => Some(RangeDatum::MinValue),
                    Some(n) if n.eq_ignore_ascii_case("maxvalue") => Some(RangeDatum::MaxValue),
                    _ => None,
                }
            }
            _ => None,
        };
        let d = match infinite {
            Some(d) => d,
            None => match bound_value(interp, v.as_ref(), key)? {
                Some(d) => RangeDatum::Value(d),
                None => {
                    return Err(DdlError::Parse("cannot specify NULL in range bound".into()));
                }
            },
        };
        out.push(d);
    }
    let mut kind: Option<&RangeDatum> = None;
    for d in &out {
        match (kind, d) {
            (Some(RangeDatum::MinValue), RangeDatum::MinValue)
            | (Some(RangeDatum::MaxValue), RangeDatum::MaxValue) => {}
            (Some(RangeDatum::MinValue), _) => {
                return Err(DdlError::Parse(
                    "every bound following MINVALUE must also be MINVALUE".into(),
                ));
            }
            (Some(RangeDatum::MaxValue), _) => {
                return Err(DdlError::Parse(
                    "every bound following MAXVALUE must also be MAXVALUE".into(),
                ));
            }
            _ => {
                if matches!(d, RangeDatum::MinValue | RangeDatum::MaxValue) {
                    kind = Some(d);
                }
            }
        }
    }
    Ok(out)
}

/// transformPartitionBoundValue: a constant expression coerced to the key
/// type. `None` is NULL.
fn bound_value(
    interp: &PgCatalog,
    v: Option<&typedpg_pg_query::ast::Node>,
    key: &PartKey,
) -> Result<Option<Datum>, DdlError> {
    let (key_type, key_name) = (&key.type_oid, &key.name);
    use crate::coerce::{CoercionContext, coercion_pathway};
    use crate::expr::{TypeGoal, infer_expr};
    use crate::pg_catalog::oid;

    if let Some(inner) = v
        && (matches!(inner, ast::Node::ColumnRef(_))
            || inner
                .nodes()
                .any(|(n, ..)| matches!(n, typedpg_pg_query::NodeRef::ColumnRef(_))))
    {
        return Err(DdlError::Parse(
            "cannot use column reference in partition bound expression".into(),
        ));
    }
    crate::ddl::expr_kind::check_expr_kind(
        interp,
        v,
        crate::ddl::expr_kind::ExprKind::PartitionBound,
    )?;
    if let Some(ast::Node::AConst(c)) = v
        && c.isnull
    {
        return Ok(None);
    }
    let scope = crate::scope::Scope::default();
    let null_ctx = crate::nullability::NullabilityContext::default();
    let mut params = crate::param_collector::ParamCollector::default();
    let ctx = || crate::expr::Ctx::new(&scope, &null_ctx, interp);
    let found = infer_expr(v, ctx(), &mut params, TypeGoal::NONE)
        .map_err(|e| DdlError::Parse(e.to_string()))?;
    if found.type_oid != oid::UNKNOWN
        && *key_type != oid::UNKNOWN
        && coercion_pathway(
            *key_type,
            found.type_oid,
            CoercionContext::Assignment,
            interp,
        )
        .is_none()
    {
        return Err(DdlError::Parse(format!(
            "specified value cannot be cast to type {} for column \"{}\"",
            format_type_for_message(interp, *key_type),
            key_name.as_deref().unwrap_or("expression")
        )));
    }
    if *key_type != oid::UNKNOWN {
        // The literal must be valid input for the key type.
        infer_expr(v, ctx(), &mut params, TypeGoal::assignment(*key_type))
            .map_err(|e| DdlError::Parse(e.to_string()))?;
    }
    // evaluate_expr runs the expression coerced to the key's type: what
    // fails there (`2147483647 + 1`, `1 / 0`) fails the statement.
    if *key_type != oid::UNKNOWN
        && let Some(err) = crate::fold::error(v, interp)
            .or_else(|| crate::fold::conversion_error(v, *key_type, key.typmod, interp))
    {
        return Err(DdlError::Parse(
            err.raw(None).finalize_implicit().to_string(),
        ));
    }
    let datum = v
        .as_ref()
        .map_or(Datum::Opaque, |v| normalize(interp, v, key));
    // The value coerced to a `varchar(n)` / `char(n)` key: too long is an
    // error unless the excess is blanks, which go.
    if let Datum::Text(t) | Datum::CodePoints(t) = &datum
        && let Some(msg) = crate::typmod::char_length_violation(
            interp,
            interp.unwrap_domain(*key_type),
            key.typmod,
            t,
        )
    {
        return Err(DdlError::Parse(msg));
    }
    // evaluate_expr: the value coerced to an integer key must fit it (the
    // cast's "smallint out of range").
    let base = interp.unwrap_domain(*key_type);
    let range = match base {
        oid::INT2 => Some((i128::from(i16::MIN), i128::from(i16::MAX))),
        oid::INT4 => Some((i128::from(i32::MIN), i128::from(i32::MAX))),
        oid::INT8 => Some((i128::from(i64::MIN), i128::from(i64::MAX))),
        _ => None,
    };
    if let (Some((lo, hi)), Datum::Int(value)) = (range, &datum)
        && !(lo..=hi).contains(value)
    {
        return Err(DdlError::Parse(format!(
            "{} out of range",
            format_type_for_message(interp, base)
        )));
    }
    Ok(Some(datum))
}

// Built-in type OIDs beyond `pg_catalog::oid` (fixed by PG's `pg_type.dat`).
const INTERVAL: PgTypeOid = PgTypeOid::from_raw(1186);
const UUID: PgTypeOid = PgTypeOid::from_raw(2950);

/// The comparable form of a bound constant (or of a row's value, read as
/// the key's type): the value evaluate_expr gives the expression coerced
/// to the key's type, where the analyzer computes it exactly.
fn normalize(interp: &PgCatalog, v: &typedpg_pg_query::ast::Node, part_key: &PartKey) -> Datum {
    use crate::pg_catalog::oid;
    if !part_key.builtin_order {
        return Datum::Opaque;
    }
    let key = interp.unwrap_domain(part_key.type_oid);
    let value = match key {
        oid::INT2 | oid::INT4 | oid::INT8 => integer_value(interp, v).map(Datum::Int),
        oid::NUMERIC => numeric_value(interp, v, part_key).map(Datum::Numeric),
        oid::FLOAT4 | oid::FLOAT8 => float_value(interp, v, key).map(Datum::Float),
        oid::BOOL => bool_value(interp, v, key).map(|b| Datum::Int(i128::from(b))),
        // date_in drops the time of day; timestamp(p) rounds the
        // fraction (AdjustTimestampForTypmod); timestamptz_in reads a
        // value without a zone in the session's TimeZone, which isn't
        // known.
        oid::DATE => key_literal(interp, v, key)
            .and_then(|s| crate::datetime_input::datetime_value(s, None))
            .map(Datum::Int),
        oid::TIMESTAMP | oid::TIMESTAMPTZ => key_literal(interp, v, key)
            .and_then(|s| crate::datetime_input::datetime_value(s, Some(key == oid::TIMESTAMPTZ)))
            .map(|t| match part_key.typmod {
                Some(p) => crate::datetime_input::round_timestamp(t, p),
                None => t,
            })
            .map(Datum::Int),
        // interval_cmp_value: a month is 30 days, a day 24 hours. A typmod
        // truncates the value (AdjustIntervalForTypmod).
        INTERVAL if part_key.typmod.is_none() => key_literal(interp, v, key)
            .and_then(crate::datetime_input::finite_interval)
            .map(|(months, days, time)| {
                Datum::Int(
                    i128::from(time)
                        + (i128::from(months) * 30 + i128::from(days)) * 86_400_000_000,
                )
            }),
        UUID => key_literal(interp, v, key)
            .and_then(uuid_value)
            .map(Datum::Int),
        _ if interp
            .get_type(key)
            .is_some_and(|t| t.typtype == crate::pg_catalog::TypType::Enum) =>
        {
            key_literal(interp, v, key)
                .and_then(|label| enum_value(interp, key, label))
                .map(Datum::Int)
        }
        _ if matches!(
            interp.pg_type.get(&key).map(|t| t.typcategory),
            Some(crate::pg_catalog::TypCategory::String)
        ) =>
        {
            string_value(interp, v, part_key, key)
        }
        _ => None,
    };
    value.unwrap_or(Datum::Opaque)
}

/// The text of an untyped string literal, or of one cast to the key's
/// own type (`'…'::date` for a date key) without a type modifier: what
/// the key type's input function reads.
fn key_literal<'a>(interp: &PgCatalog, v: &'a ast::Node, key: PgTypeOid) -> Option<&'a str> {
    let literal = |n: &'a ast::Node| match n {
        ast::Node::AConst(c) if !c.isnull => match &c.val {
            Some(ast::AConstValue::String(s)) => Some(s.sval.as_str()),
            _ => None,
        },
        _ => None,
    };
    match v {
        ast::Node::TypeCast(tc)
            if crate::nonnull::cast_target(tc, interp).map(|t| interp.unwrap_domain(t))
                == Some(key) =>
        {
            literal(tc.arg.as_ref()?)
        }
        _ => literal(v),
    }
}

/// An integer key's value: an untyped literal read by int2in / int4in /
/// int8in, or the constant expression folded ([`crate::fold`]) — a
/// `numeric` one rounded, halves away from zero, as numeric_int4 does.
/// The key type's range is checked by the caller.
fn integer_value(interp: &PgCatalog, v: &ast::Node) -> Option<i128> {
    if let ast::Node::AConst(c) = v
        && let Some(ast::AConstValue::String(s)) = &c.val
    {
        return crate::literal_input::parse_pg_integer(&s.sval);
    }
    match crate::fold::fold(Some(v), interp)?.ok()? {
        crate::fold::Value::Int(i, _) => Some(i),
        crate::fold::Value::Numeric(raw) => round_numeric(&raw),
    }
}

/// A `numeric` key's value, under the key's type modifier.
fn numeric_value(interp: &PgCatalog, v: &ast::Node, key: &PartKey) -> Option<NumericKey> {
    let value = match v {
        ast::Node::AConst(c) if matches!(c.val, Some(ast::AConstValue::String(_))) => {
            let Some(ast::AConstValue::String(s)) = &c.val else {
                return None;
            };
            crate::literal_input::validate_numeric(&s.sval).ok()?;
            crate::fold::Value::Numeric(s.sval.clone())
        }
        _ => crate::fold::fold(Some(v), interp)?.ok()?,
    };
    let value = crate::fold::convert(&value, key.type_oid, key.typmod, interp)?.ok()?;
    NumericKey::of(&value)
}

/// A `float4` / `float8` key's value: an untyped literal read by float4in /
/// float8in, or an integer or `numeric` constant converted (int4_float8,
/// numeric_float8 — which reads the numeric's text).
fn float_value(interp: &PgCatalog, v: &ast::Node, key: PgTypeOid) -> Option<u64> {
    let single = key == crate::pg_catalog::oid::FLOAT4;
    let read = |s: &str| -> Option<f64> {
        let s = s.trim_matches(|c: char| c.is_ascii_whitespace());
        if single {
            s.parse::<f32>().ok().map(f64::from)
        } else {
            s.parse::<f64>().ok()
        }
    };
    let x = match key_literal(interp, v, key) {
        Some(s) => read(s)?,
        None => match crate::fold::fold(Some(v), interp)?.ok()? {
            crate::fold::Value::Int(i, _) => {
                let i = i64::try_from(i).ok()?;
                if single {
                    f64::from(i as f32)
                } else {
                    i as f64
                }
            }
            crate::fold::Value::Numeric(raw) => read(&raw)?,
        },
    };
    Some(float_key(x))
}

/// A boolean key's value: TRUE / FALSE, or a string boolin reads.
fn bool_value(interp: &PgCatalog, v: &ast::Node, key: PgTypeOid) -> Option<bool> {
    if let ast::Node::AConst(c) = v
        && let Some(ast::AConstValue::Boolean(b)) = &c.val
    {
        return Some(b.boolval);
    }
    crate::ddl::reloptions::parse_bool(key_literal(interp, v, key)?)
}

/// uuid_in: 32 hex digits, a hyphen allowed after each group of four,
/// optionally in braces — as an integer in uuid_cmp's (byte) order.
fn uuid_value(s: &str) -> Option<i128> {
    let b = s.as_bytes();
    let (mut i, braces) = if b.first() == Some(&b'{') {
        (1, true)
    } else {
        (0, false)
    };
    let mut value: u128 = 0;
    for byte in 0..16 {
        let hi = char::from(*b.get(i)?).to_digit(16)?;
        let lo = char::from(*b.get(i + 1)?).to_digit(16)?;
        value = value << 8 | u128::from(hi << 4 | lo);
        i += 2;
        if b.get(i) == Some(&b'-') && byte % 2 == 1 && byte < 15 {
            i += 1;
        }
    }
    if braces {
        (b.get(i) == Some(&b'}')).then_some(())?;
        i += 1;
    }
    (i == b.len()).then_some((value ^ 1 << 127) as i128)
}

/// An enum key's value: its label's sort order (enum_cmp) — unless another
/// label shares it, which the analyzer's sort orders can't tell apart.
fn enum_value(interp: &PgCatalog, key: PgTypeOid, label: &str) -> Option<i128> {
    let labels = interp.pg_enum.get(&key)?;
    let order = labels.iter().find(|e| e.enumlabel == label)?.enumsortorder;
    (labels.iter().filter(|e| e.enumsortorder == order).count() == 1)
        .then(|| i128::from(float_key(f64::from(order))))
}

/// A string key's value: an untyped literal, or one cast to a type that
/// keeps it as the key's (the key's own type, text / varchar for a text
/// or varchar key — not through a type modifier, nor through `bpchar`,
/// whose trailing blanks a text key loses) — fitted to the key's typmod.
fn string_value(
    interp: &PgCatalog,
    v: &ast::Node,
    part_key: &PartKey,
    key: PgTypeOid,
) -> Option<Datum> {
    use crate::pg_catalog::oid;
    let keeps_value = |tc: &typedpg_pg_query::ast::TypeCast| {
        let Some(t) = crate::nonnull::cast_target(tc, interp).map(|t| interp.unwrap_domain(t))
        else {
            return false;
        };
        let texts = [oid::TEXT, oid::VARCHAR];
        t == key || (texts.contains(&key) && texts.contains(&t))
    };
    let literal = match v {
        ast::Node::AConst(c) => c.val.as_ref(),
        ast::Node::TypeCast(tc) if keeps_value(tc) => match tc.arg.as_ref() {
            Some(ast::Node::AConst(c)) => c.val.as_ref(),
            _ => None,
        },
        _ => None,
    };
    let Some(ast::AConstValue::String(s)) = literal else {
        return None;
    };
    // The key's typmod: a `varchar(n)` / `char(n)` value loses the blanks
    // past its `n`th character (more is an error, see `bound_value`); a
    // `char(n)` value compares without its trailing blanks.
    let s = &s.sval;
    let n = match crate::typmod::decode(interp, key, part_key.typmod) {
        crate::typmod::DecodedTypmod::Length(n) => usize::try_from(n).ok(),
        _ => None,
    };
    let fit: String = match n {
        Some(n)
            if crate::typmod::char_length_violation(interp, key, part_key.typmod, s).is_none() =>
        {
            s.chars().take(n).collect()
        }
        _ => s.to_owned(),
    };
    let fit = if key == oid::BPCHAR {
        fit.trim_end_matches(' ').to_owned()
    } else {
        fit
    };
    Some(if part_key.code_point_order {
        Datum::CodePoints(fit)
    } else {
        Datum::Text(fit)
    })
}

/// A numeric constant (`1.5`, `-2.49`, `12e3`) rounded to an integer, half
/// away from zero, as numeric_int4 does.
fn round_numeric(text: &str) -> Option<i128> {
    let (mantissa, exponent) = match text.split_once(['e', 'E']) {
        Some((m, e)) => (m, e.parse::<i32>().ok()?),
        None => (text, 0),
    };
    let negative = mantissa.starts_with('-');
    let mantissa = mantissa.trim_start_matches(['-', '+']);
    let (int_part, frac_part) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let mut digits: String = format!("{int_part}{frac_part}");
    // The position of the decimal point within `digits`.
    let point = i64::try_from(int_part.len()).ok()? + i64::from(exponent);
    if point > 38 {
        return None;
    }
    if point < 0 {
        return Some(0);
    }
    let point = usize::try_from(point).ok()?;
    while digits.len() < point {
        digits.push('0');
    }
    let (whole, rest) = digits.split_at(point);
    let mut value: i128 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    if rest.as_bytes().first().is_some_and(|d| *d >= b'5') {
        value += 1;
    }
    Some(if negative { -value } else { value })
}

/// Whether two values of one key are ordered by the analyzer (a string
/// under a collation not ordering by code point is compared for equality
/// only).
fn orderable(d: &Datum) -> bool {
    matches!(
        d,
        Datum::Int(_) | Datum::Numeric(_) | Datum::Float(_) | Datum::CodePoints(_)
    )
}

/// Compare two range bound tuples: `None` when some value can't be
/// ordered.
fn cmp_range(a: &[RangeDatum], b: &[RangeDatum]) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    for (x, y) in a.iter().zip(b) {
        let rank = |d: &RangeDatum| match d {
            RangeDatum::MinValue => 0,
            RangeDatum::Value(_) => 1,
            RangeDatum::MaxValue => 2,
        };
        match (x, y) {
            (RangeDatum::Value(Datum::Opaque), _) | (_, RangeDatum::Value(Datum::Opaque)) => {
                return None;
            }
            (RangeDatum::Value(p), RangeDatum::Value(q)) => {
                if !orderable(p) || std::mem::discriminant(p) != std::mem::discriminant(q) {
                    return None;
                }
                match p.cmp(q) {
                    Ordering::Equal => continue,
                    o => return Some(o),
                }
            }
            _ => match rank(x).cmp(&rank(y)) {
                // Two equal infinities end the comparison.
                Ordering::Equal => return Some(Ordering::Equal),
                o => return Some(o),
            },
        }
    }
    Some(Ordering::Equal)
}

fn check_new_bound(
    interp: &PgCatalog,
    parent: PgClassOid,
    part: PgClassOid,
    bound: &Bound,
) -> Result<(), DdlError> {
    let part_name = relname_of(interp, part);
    let siblings: Vec<(PgClassOid, &Bound)> = inherit::children_of(interp, parent)
        .into_iter()
        .filter(|&c| c != part)
        .filter_map(|c| interp.partition_bounds.get(&c).map(|b| (c, b)))
        .collect();
    let overlap = |other: PgClassOid| {
        DdlError::Parse(format!(
            "partition \"{part_name}\" would overlap partition \"{}\"",
            relname_of(interp, other)
        ))
    };
    match bound {
        Bound::Default => {
            if let Some((other, _)) = siblings.iter().find(|(_, b)| matches!(b, Bound::Default)) {
                return Err(DdlError::Parse(format!(
                    "partition \"{part_name}\" conflicts with existing default partition \"{}\"",
                    relname_of(interp, *other)
                )));
            }
        }
        Bound::List(values) => {
            for value in values {
                if value.as_ref().is_some_and(|d| *d == Datum::Opaque) {
                    continue;
                }
                for (other, b) in &siblings {
                    if let Bound::List(theirs) = b
                        && theirs.contains(value)
                    {
                        return Err(overlap(*other));
                    }
                }
            }
        }
        Bound::Range { lower, upper } => {
            if cmp_range(lower, upper).is_some_and(|o| o != std::cmp::Ordering::Less) {
                return Err(DdlError::Parse(format!(
                    "empty range bound specified for partition \"{part_name}\""
                )));
            }
            // check_new_partition_bound reports the partition its bound
            // search lands on: the one holding the new lower bound, else
            // the first after it — of those overlapping, the lowest.
            let overlapping = siblings.iter().filter_map(|(other, b)| match b {
                Bound::Range {
                    lower: their_lower,
                    upper: their_upper,
                } if cmp_range(lower, their_upper) == Some(std::cmp::Ordering::Less)
                    && cmp_range(their_lower, upper) == Some(std::cmp::Ordering::Less) =>
                {
                    Some((*other, their_lower))
                }
                _ => None,
            });
            if let Some((other, _)) = overlapping
                .min_by(|(_, a), (_, b)| cmp_range(a, b).unwrap_or(std::cmp::Ordering::Equal))
            {
                return Err(overlap(other));
            }
        }
        Bound::Hash { modulus, remainder } => {
            // Every modulus must divide the next larger one.
            let mut hashes: Vec<(i32, i32, PgClassOid)> = siblings
                .iter()
                .filter_map(|(c, b)| match b {
                    Bound::Hash { modulus, remainder } => Some((*modulus, *remainder, *c)),
                    _ => None,
                })
                .collect();
            hashes.sort();
            let factor_err = |detail: String| {
                DdlError::Parse(format!(
                    "every hash partition modulus must be a factor of the next larger modulus \
                     ({detail})"
                ))
            };
            if let Some(&(greater, _, other)) = hashes.iter().find(|(m, ..)| m > modulus)
                && greater % modulus != 0
            {
                return Err(factor_err(format!(
                    "The new modulus {modulus} is not a factor of {greater}, the modulus of \
                     existing partition \"{}\".",
                    relname_of(interp, other)
                )));
            }
            if let Some(&(lesser, _, other)) = hashes.iter().rev().find(|(m, ..)| m <= modulus)
                && modulus % lesser != 0
            {
                return Err(factor_err(format!(
                    "The new modulus {modulus} is not divisible by {lesser}, the modulus of \
                     existing partition \"{}\".",
                    relname_of(interp, other)
                )));
            }
            for &(m, r, other) in &hashes {
                let collide = if *modulus >= m {
                    remainder % m == r
                } else {
                    r % modulus == *remainder
                };
                if collide {
                    return Err(overlap(other));
                }
            }
        }
    }
    Ok(())
}

/// What the partition constraint of `part` (get_qual_from_partbound) says
/// of its rows, by the attnums of `parent`'s columns.
pub(crate) struct BoundFacts {
    /// Columns never NULL: every key a range bound is over (`key IS NOT
    /// NULL` for each, MINVALUE / MAXVALUE ones too), or a list bound's
    /// key when the list has no NULL — a key expression's strict columns.
    pub not_null: Vec<i16>,
    /// A list bound over a plain column: the values a non-NULL key is one
    /// of — when the key's equality is the column type's own (integers,
    /// or text / varchar under the column's collation), so a value equal
    /// to one of them by the key is so by the column's `=` too.
    pub values: Option<(i16, Vec<crate::nonnull::Literal>)>,
}

/// [`BoundFacts`] of partition `part` of `parent`. `None` for a default
/// or hash partition, which may hold any key (a NULL one included).
pub(crate) fn bound_facts(
    interp: &PgCatalog,
    parent: PgClassOid,
    part: PgClassOid,
) -> Option<BoundFacts> {
    use crate::nonnull::{LitKind, Literal};
    use crate::pg_catalog::oid;
    let spec = interp.partition_specs.get(&parent)?;
    match interp.partition_bounds.get(&part)? {
        Bound::Default | Bound::Hash { .. } => None,
        Bound::Range { .. } => Some(BoundFacts {
            not_null: spec
                .keys
                .iter()
                .flat_map(|k| k.strict_attnums.iter().copied())
                .collect(),
            values: None,
        }),
        Bound::List(values) => {
            let key = spec.keys.first()?;
            let has_null = values.iter().any(Option::is_none);
            let column = key
                .name
                .as_ref()
                .and_then(|_| key.strict_attnums.first().copied());
            let attr = column.and_then(|n| {
                interp
                    .attributes_of(parent)
                    .iter()
                    .find(|a| a.attnum == n)
                    .cloned()
            });
            let eqop = partition_key_eqops(interp, parent)
                .first()
                .copied()
                .flatten()
                .map(|o| o.get());
            let literals = attr.and_then(|a| {
                let base = interp.unwrap_domain(key.type_oid);
                let integer = [oid::INT2, oid::INT4, oid::INT8].contains(&base)
                    && matches!(eqop, Some(94 | 96 | 410));
                // texteq, under the column's own collation.
                let text = [oid::TEXT, oid::VARCHAR].contains(&base)
                    && eqop == Some(98)
                    && key.collation == a.attcollation;
                let lits: Option<Vec<Literal>> = values
                    .iter()
                    .flatten()
                    .map(|d| match d {
                        Datum::Int(i) if integer => Some(Literal {
                            text: i.to_string(),
                            kind: LitKind::Integer,
                        }),
                        Datum::Text(s) | Datum::CodePoints(s) if text => Some(Literal {
                            text: s.clone(),
                            kind: LitKind::String,
                        }),
                        _ => None,
                    })
                    .collect();
                lits.map(|l| (a.attnum, l))
            });
            Some(BoundFacts {
                not_null: if has_null {
                    Vec::new()
                } else {
                    key.strict_attnums.clone()
                },
                values: literals,
            })
        }
    }
}

/// Where ExecFindPartition sends a row inserted into a relation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    /// The leaf partition the row lands in (the relation itself when it is
    /// not partitioned).
    Leaf(PgClassOid),
    /// No partition of this partitioned table (the root or a
    /// sub-partition the row was routed to) takes the row:
    /// `no partition of relation "…" found for row`.
    NoPartition(PgClassOid),
    /// The analyzer can't tell.
    Unknown,
}

impl Route {
    /// The leaf, when known.
    pub(crate) fn leaf(self) -> Option<PgClassOid> {
        match self {
            Route::Leaf(oid) => Some(oid),
            _ => None,
        }
    }
}

/// Where ExecFindPartition routes a row inserted into `relid`, through
/// every level of sub-partitioning — where the analyzer can tell: each key
/// a column whose value `value_of` gives (`Some(None)`: NULL), compared as
/// the bounds are. No partition takes the row only when it was compared
/// with every bound of its level and there is no default partition.
pub(crate) fn route_row<'a>(
    interp: &PgCatalog,
    relid: PgClassOid,
    value_of: &dyn Fn(&str) -> Option<Option<&'a typedpg_pg_query::ast::Node>>,
) -> Route {
    route_row_opt(interp, relid, value_of).unwrap_or(Route::Unknown)
}

/// [`route_row`], `None` standing for [`Route::Unknown`].
fn route_row_opt<'a>(
    interp: &PgCatalog,
    relid: PgClassOid,
    value_of: &dyn Fn(&str) -> Option<Option<&'a typedpg_pg_query::ast::Node>>,
) -> Option<Route> {
    let Some(spec) = interp.partition_specs.get(&relid) else {
        return Some(Route::Leaf(relid));
    };
    let mut row: Vec<Option<Datum>> = Vec::new();
    for key in &spec.keys {
        let value = match value_of(key.name.as_deref()?)? {
            None => None,
            // A domain's CHECK may reject the value as the row is built.
            Some(_) if interp.domain_has_checks(key.type_oid) => return None,
            Some(v) => match normalize(interp, v, key) {
                Datum::Opaque => return None,
                // Out of the key type's range: PG fails on that first
                // (`integer out of range`).
                Datum::Int(i) if !int_fits(interp.unwrap_domain(key.type_oid), i) => {
                    return None;
                }
                // Equality of strings is byte equality under a deterministic
                // collation — every built-in one; a user's may not be.
                Datum::Text(_)
                    if key.collation.is_some_and(|c| {
                        interp.pg_collation.get(&c).is_none_or(|c| {
                            interp.namespace_name(c.collnamespace) != Some("pg_catalog")
                        })
                    }) =>
                {
                    return None;
                }
                d => Some(d),
            },
        };
        row.push(value);
    }
    let mut bounds = Vec::new();
    for child in inherit::children_of(interp, relid) {
        bounds.push((child, interp.partition_bounds.get(&child)?));
    }
    let default = || {
        bounds
            .iter()
            .find(|(_, b)| matches!(b, Bound::Default))
            .map(|(c, _)| *c)
    };
    // Whether every bound compared with the row: no partition taking it
    // then means none does.
    let mut decided = true;
    let mut found = None;
    match spec.strategy {
        Strategy::Hash => return None,
        Strategy::List => {
            let value = row.first()?;
            for (child, bound) in &bounds {
                let Bound::List(values) = bound else { continue };
                if values.contains(value) {
                    found = Some(*child);
                    break;
                }
                decided &= !values.contains(&Some(Datum::Opaque));
            }
        }
        // A NULL in a range key goes to the default partition.
        Strategy::Range if row.iter().any(Option::is_none) => {}
        Strategy::Range => {
            let row: Vec<RangeDatum> = row.into_iter().flatten().map(RangeDatum::Value).collect();
            for (child, bound) in &bounds {
                let Bound::Range { lower, upper } = bound else {
                    continue;
                };
                match (cmp_range(lower, &row), cmp_range(&row, upper)) {
                    (Some(lo), Some(hi)) => {
                        if lo != std::cmp::Ordering::Greater && hi == std::cmp::Ordering::Less {
                            found = Some(*child);
                            break;
                        }
                    }
                    _ => decided = false,
                }
            }
        }
    }
    let target = match found {
        Some(child) => child,
        None if decided => match default() {
            Some(child) => child,
            // get_partition_for_tuple found no partition, and there is no
            // default one to fall back on.
            None => return Some(Route::NoPartition(relid)),
        },
        None => return None,
    };
    route_row_opt(interp, target, value_of)
}

/// Whether integer `i` is a value of integer type `ty` (any other type:
/// true — its values are not integers).
fn int_fits(ty: PgTypeOid, i: i128) -> bool {
    use crate::pg_catalog::oid;
    match ty {
        t if t == oid::INT2 => i16::try_from(i).is_ok(),
        t if t == oid::INT4 => i32::try_from(i).is_ok(),
        t if t == oid::INT8 => i64::try_from(i).is_ok(),
        _ => true,
    }
}

/// The partitions of `parent` in PartitionDesc order (partition_bounds_create):
/// range partitions by lower bound, list partitions by their smallest
/// non-NULL value (a NULL-only one after those), hash partitions by
/// (modulus, remainder), the default partition last. Bounds the analyzer
/// can't order (text values under a collation other than code point
/// order) come after the ordered ones, in creation order.
pub(crate) fn partition_desc_order(interp: &PgCatalog, parent: PgClassOid) -> Vec<PgClassOid> {
    // A total order: (group, orderable key, creation order).
    let key = |p: &PgClassOid| -> (u8, Option<Vec<(u8, Datum)>>, PgClassOid) {
        match interp.partition_bounds.get(p) {
            Some(Bound::Default) => (3, None, *p),
            Some(Bound::List(values)) if values.iter().all(Option::is_none) => (2, None, *p),
            Some(Bound::List(values)) => {
                let min = values.iter().flatten().filter(|d| orderable(d)).min();
                let all = values.iter().flatten().all(orderable);
                (0, min.filter(|_| all).map(|d| vec![(1, d.clone())]), *p)
            }
            Some(Bound::Range { lower, .. }) => {
                let parts: Option<Vec<(u8, Datum)>> = lower
                    .iter()
                    .map(|d| match d {
                        RangeDatum::MinValue => Some((0, Datum::Opaque)),
                        RangeDatum::Value(v) if orderable(v) => Some((1, v.clone())),
                        RangeDatum::Value(_) => None,
                        RangeDatum::MaxValue => Some((2, Datum::Opaque)),
                    })
                    .collect();
                (0, parts, *p)
            }
            Some(Bound::Hash { modulus, remainder }) => (
                0,
                Some(vec![
                    (1, Datum::Int(i128::from(*modulus))),
                    (1, Datum::Int(i128::from(*remainder))),
                ]),
                *p,
            ),
            None => (1, None, *p),
        }
    };
    let mut parts = inherit::children_of(interp, parent);
    parts.sort_by_key(|p| {
        let (group, k, oid) = key(p);
        (group, k.is_none(), k, oid)
    });
    parts
}
