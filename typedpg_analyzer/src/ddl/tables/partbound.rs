//! Partition bounds (`FOR VALUES ...` / `DEFAULT`): PG validates a new
//! partition's bound against the parent's strategy and key types
//! (`transformPartitionBound`) and against the sibling partitions
//! (`check_new_partition_bound`: overlaps, one default partition, hash
//! moduli dividing each other).
//!
//! Values are compared only when their order is known without a PG
//! runtime: integer keys, date / timestamp keys written in ISO form, and
//! string keys under a collation that orders by code point (for LIST also
//! any string key, by equality). Other values never overlap as far as the
//! analyzer can tell.

use super::*;
use typedpg_pg_query::protobuf::PartitionBoundSpec;

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
    names: &[typedpg_pg_query::protobuf::Node],
) -> Option<crate::oid::PgCollationOid> {
    let names: Vec<&str> = names
        .iter()
        .filter_map(super::super::util::node_string)
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

/// The collations whose order is the strings' code point order: C / POSIX
/// (byte order, which for UTF-8 is code point order), `ucs_basic`, and the
/// builtin provider's `pg_c_utf8` / `pg_unicode_fast`.
fn orders_by_code_point(interp: &PgCatalog, collation: crate::oid::PgCollationOid) -> bool {
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

/// A bound value, as far as the analyzer can order it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Datum {
    Int(i128),
    /// A normalized ISO date / timestamp: its text order is its time order.
    Stamp(String),
    /// A string key value under a collation that orders by code point.
    CodePoints(String),
    /// A string key value, compared by equality only (its order depends
    /// on the collation).
    Text(String),
    /// Anything else: equal to nothing, ordered against nothing.
    Opaque,
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
    spec: &typedpg_pg_query::protobuf::PartitionSpec,
) {
    use typedpg_pg_query::protobuf::PartitionStrategy as Ps;
    let strategy = match Ps::try_from(spec.strategy) {
        Ok(Ps::List) => Strategy::List,
        Ok(Ps::Hash) => Strategy::Hash,
        _ => Strategy::Range,
    };
    let am = if strategy == Strategy::Hash {
        "hash"
    } else {
        "btree"
    };
    // ComputePartitionAttrs: the key's operator class, as written or the
    // type's default (its errors were reported when the key was checked).
    let opclass_of = |typ: PgTypeOid, pe: &typedpg_pg_query::protobuf::PartitionElem| {
        crate::ddl::opclass::resolve_index_opclass(interp, &pe.opclass, typ, am)
            .ok()
            .flatten()
    };
    let mut keys = Vec::new();
    for elem in &spec.part_params {
        let Some(node::Node::PartitionElem(pe)) = elem.node.as_ref() else {
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
                opclass,
                strict_attnums: attr.map(|a| a.attnum).into_iter().collect(),
                typmod: attr.and_then(|a| interp.effective_typmod(a.atttypid, a.atttypmod)),
            });
        } else if let Some(expr) = pe.expr.as_deref() {
            let typ = match crate::ddl::volatile::infer_over_relation(interp, relid, expr, None) {
                Some(Ok(t)) => t.type_oid,
                _ => crate::pg_catalog::oid::UNKNOWN,
            };
            let opclass = opclass_of(typ, pe);
            let strict_attnums =
                crate::ddl::volatile::strict_columns_over_relation(interp, relid, expr)
                    .iter()
                    .filter_map(|c| interp.attribute_by_name(relid, c).map(|a| a.attnum))
                    .collect();
            keys.push(PartKey {
                type_oid: typ,
                name: None,
                collation: explicit.flatten(),
                code_point_order: explicit
                    .flatten()
                    .is_some_and(|c| orders_by_code_point(interp, c))
                    || compares_bytes(interp, opclass),
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
                opclass: None,
                strict_attnums: Vec::new(),
                typmod: None,
            });
            let mut values: Vec<Option<Datum>> = Vec::new();
            for v in &spec.listdatums {
                let d = bound_value(interp, v, &key)?;
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
    values: &[typedpg_pg_query::protobuf::Node],
    keys: &[PartKey],
) -> Result<Vec<RangeDatum>, DdlError> {
    let mut out = Vec::new();
    for (v, key) in values.iter().zip(keys) {
        let infinite = match v.node.as_ref() {
            Some(node::Node::ColumnRef(cr)) if cr.fields.len() == 1 => {
                match cr.fields.first().and_then(super::super::util::node_string) {
                    Some(n) if n.eq_ignore_ascii_case("minvalue") => Some(RangeDatum::MinValue),
                    Some(n) if n.eq_ignore_ascii_case("maxvalue") => Some(RangeDatum::MaxValue),
                    _ => None,
                }
            }
            _ => None,
        };
        let d = match infinite {
            Some(d) => d,
            None => match bound_value(interp, v, key)? {
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
    v: &typedpg_pg_query::protobuf::Node,
    key: &PartKey,
) -> Result<Option<Datum>, DdlError> {
    let (key_type, key_name) = (&key.type_oid, &key.name);
    use crate::coerce::{CoercionContext, coercion_pathway};
    use crate::expr::{TypeGoal, infer_expr};
    use crate::pg_catalog::oid;

    if let Some(inner) = v.node.as_ref()
        && (matches!(inner, node::Node::ColumnRef(_))
            || inner
                .nodes()
                .into_iter()
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
    if let Some(node::Node::AConst(c)) = v.node.as_ref()
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
    let datum = match normalize(interp, v, key) {
        // The value coerced to a `varchar(n)` / `char(n)` key: too long
        // is an error unless the excess is blanks, which go.
        Datum::Text(t) | Datum::CodePoints(t)
            if let Some(msg) = crate::typmod::char_length_violation(
                interp,
                interp.unwrap_domain(*key_type),
                key.typmod,
                &t,
            ) =>
        {
            return Err(DdlError::Parse(msg));
        }
        d => d,
    };
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

/// The comparable form of a bound constant.
fn normalize(
    interp: &PgCatalog,
    v: &typedpg_pg_query::protobuf::Node,
    part_key: &PartKey,
) -> Datum {
    use crate::pg_catalog::oid;
    let key = interp.unwrap_domain(part_key.type_oid);
    let integer_key = [oid::INT2, oid::INT4, oid::INT8].contains(&key);
    let datetime_key = [oid::DATE, oid::TIMESTAMP, oid::TIMESTAMPTZ].contains(&key);
    let text_key = matches!(
        interp.pg_type.get(&key).map(|t| t.typcategory),
        Some(crate::pg_catalog::TypCategory::String)
    );
    let bool_key = key == oid::BOOL;
    // A constant cast to a type that keeps its value as the key's: the
    // key's own type, another integer type (or numeric) for an integer
    // key, text / varchar for a text or varchar one. Not through a type
    // modifier (`'bc'::char(1)` is `'b'`, `15::numeric(1,-1)` is 20), nor
    // through `bpchar` (whose trailing blanks a text key loses) or a date
    // (a timestamp key's time of day).
    let keeps_value = |tc: &typedpg_pg_query::protobuf::TypeCast| {
        let Some(t) = crate::nonnull::cast_target(tc, interp).map(|t| interp.unwrap_domain(t))
        else {
            return false;
        };
        let texts = [oid::TEXT, oid::VARCHAR];
        t == key
            || (integer_key && [oid::INT2, oid::INT4, oid::INT8, oid::NUMERIC].contains(&t))
            || (texts.contains(&key) && texts.contains(&t))
    };
    let literal = match v.node.as_ref() {
        Some(node::Node::AConst(c)) => c.val.as_ref(),
        Some(node::Node::TypeCast(tc)) if keeps_value(tc) => {
            match tc.arg.as_deref().and_then(|a| a.node.as_ref()) {
                Some(node::Node::AConst(c)) => c.val.as_ref(),
                _ => None,
            }
        }
        _ => None,
    };
    // The key's typmod: a `varchar(n)` / `char(n)` value loses the blanks
    // past its `n`th character (more is an error, see `bound_value`); a
    // `char(n)` value compares without its trailing blanks.
    let fitted = |s: &str| -> String {
        let n = match crate::typmod::decode(interp, key, part_key.typmod) {
            crate::typmod::DecodedTypmod::Length(n) => usize::try_from(n).ok(),
            _ => None,
        };
        let fit: String = match n {
            Some(n)
                if crate::typmod::char_length_violation(interp, key, part_key.typmod, s)
                    .is_none() =>
            {
                s.chars().take(n).collect()
            }
            _ => s.to_owned(),
        };
        if key == oid::BPCHAR {
            fit.trim_end_matches(' ').to_owned()
        } else {
            fit
        }
    };
    if integer_key && let Some(i) = fold_integer(v) {
        return Datum::Int(i);
    }
    use typedpg_pg_query::protobuf::a_const::Val;
    match literal {
        Some(Val::Ival(i)) if integer_key => Datum::Int(i128::from(i.ival)),
        // int2in / int4in / int8in.
        Some(Val::Sval(s)) if integer_key => {
            crate::literal_input::parse_pg_integer(&s.sval).map_or(Datum::Opaque, Datum::Int)
        }
        // A numeric constant cast to an integer rounds half away from zero.
        Some(Val::Fval(f)) if integer_key => {
            round_numeric(&f.fval).map_or(Datum::Opaque, Datum::Int)
        }
        // boolin: the values of one truth are one bound value.
        Some(Val::Boolval(b)) if bool_key => Datum::Int(i128::from(b.boolval)),
        Some(Val::Sval(s)) if bool_key => crate::ddl::reloptions::parse_bool(&s.sval)
            .map_or(Datum::Opaque, |b| Datum::Int(i128::from(b))),
        Some(Val::Sval(s)) if datetime_key => {
            iso_datetime(&s.sval).map_or(Datum::Opaque, Datum::Stamp)
        }
        Some(Val::Sval(s)) if text_key && part_key.code_point_order => {
            Datum::CodePoints(fitted(&s.sval))
        }
        Some(Val::Sval(s)) if text_key => Datum::Text(fitted(&s.sval)),
        _ => Datum::Opaque,
    }
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

/// Evaluate an integer constant expression of `+`, `-`, `*` over integer
/// literals (evaluate_expr for the common case).
fn fold_integer(v: &typedpg_pg_query::protobuf::Node) -> Option<i128> {
    use typedpg_pg_query::protobuf::a_const::Val;
    match v.node.as_ref()? {
        node::Node::AConst(c) => match c.val.as_ref()? {
            Val::Ival(i) => Some(i128::from(i.ival)),
            Val::Fval(f) if !f.fval.contains(['.', 'e', 'E']) => f.fval.parse().ok(),
            _ => None,
        },
        node::Node::AExpr(e) if e.kind == typedpg_pg_query::protobuf::AExprKind::AexprOp as i32 => {
            let op = e.name.first().and_then(super::super::util::node_string)?;
            let r = fold_integer(e.rexpr.as_deref()?)?;
            match e.lexpr.as_deref() {
                None if op == "-" => Some(-r),
                None if op == "+" => Some(r),
                Some(l) => {
                    let l = fold_integer(l)?;
                    match op {
                        "+" => l.checked_add(r),
                        "-" => l.checked_sub(r),
                        "*" => l.checked_mul(r),
                        _ => None,
                    }
                }
                None => None,
            }
        }
        _ => None,
    }
}

/// `YYYY-MM-DD[( |T)HH:MM[:SS]]`, normalized to `YYYY-MM-DD HH:MM:SS` so
/// the text order is the time order.
fn iso_datetime(s: &str) -> Option<String> {
    let s = s.trim();
    let digits = |p: &str, n: usize| p.len() == n && p.bytes().all(|b| b.is_ascii_digit());
    let (date, time) = match s.split_once([' ', 'T']) {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut dparts = date.split('-');
    let (y, m, d) = (dparts.next()?, dparts.next()?, dparts.next()?);
    if dparts.next().is_some() || !digits(y, 4) || !digits(m, 2) || !digits(d, 2) {
        return None;
    }
    let time = match time {
        None => "00:00:00".to_owned(),
        Some(t) => {
            let parts: Vec<&str> = t.split(':').collect();
            match parts.as_slice() {
                [h, mi] if digits(h, 2) && digits(mi, 2) => format!("{h}:{mi}:00"),
                [h, mi, se] if digits(h, 2) && digits(mi, 2) && digits(se, 2) => {
                    format!("{h}:{mi}:{se}")
                }
                _ => return None,
            }
        }
    };
    Some(format!("{y}-{m}-{d} {time}"))
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
            (RangeDatum::Value(p), RangeDatum::Value(q)) => match (p, q) {
                (Datum::Int(_), Datum::Int(_))
                | (Datum::Stamp(_), Datum::Stamp(_))
                | (Datum::CodePoints(_), Datum::CodePoints(_)) => match p.cmp(q) {
                    Ordering::Equal => continue,
                    o => return Some(o),
                },
                _ => return None,
            },
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
            for (other, b) in &siblings {
                if let Bound::Range {
                    lower: their_lower,
                    upper: their_upper,
                } = b
                    && cmp_range(lower, their_upper) == Some(std::cmp::Ordering::Less)
                    && cmp_range(their_lower, upper) == Some(std::cmp::Ordering::Less)
                {
                    return Err(overlap(*other));
                }
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

/// The partition constraint of `part` (get_qual_from_partbound) as a CHECK
/// expression over its columns, and the columns it reads — what
/// DetachAddConstraintIfNeeded gives a partition detached CONCURRENTLY.
/// The bound values are rendered as NULLs of the key types: the analyzer
/// reads a CHECK's shape, columns and types, not its values. A key
/// expression isn't kept, so its columns stand in for it. `None` for a
/// default or hash partition.
pub(crate) fn partition_constraint_check(
    interp: &PgCatalog,
    parent: PgClassOid,
    part: PgClassOid,
) -> Option<(typedpg_pg_query::protobuf::Node, Vec<String>)> {
    let spec = interp.partition_specs.get(&parent)?;
    let bound = interp.partition_bounds.get(&part)?;
    let quote = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let typed_null = |t: PgTypeOid| {
        let typ = interp.pg_type.get(&t);
        let schema = typ
            .and_then(|t| interp.namespace_name(t.typnamespace))
            .unwrap_or("pg_catalog");
        let name = typ.map_or("unknown", |t| t.typname.as_str());
        format!("NULL::{}.{}", quote(schema), quote(name))
    };
    let mut vars: Vec<String> = Vec::new();
    let mut keys: Vec<(String, &PartKey)> = Vec::new();
    for key in &spec.keys {
        match &key.name {
            Some(name) => {
                if !vars.contains(name) {
                    vars.push(name.clone());
                }
                keys.push((quote(name), key));
            }
            None => {
                let read: Vec<String> = interp
                    .partition_key_attrs
                    .get(&parent)
                    .into_iter()
                    .flatten()
                    .filter_map(|&an| {
                        interp
                            .attributes_of(parent)
                            .iter()
                            .find(|a| a.attnum == an)
                            .map(|a| a.attname.clone())
                    })
                    .collect();
                for name in &read {
                    if !vars.contains(name) {
                        vars.push(name.clone());
                    }
                }
                let cols: Vec<String> = read.iter().map(|n| quote(n)).collect();
                keys.push((format!("ROW({})", cols.join(", ")), key));
            }
        }
    }
    let mut conjuncts: Vec<String> = Vec::new();
    match bound {
        // A hash partition's constraint (satisfies_hash_partition over the
        // parent's OID) isn't carried over: PG 18 adds no CHECK for one.
        Bound::Default | Bound::Hash { .. } => return None,
        Bound::List(values) => {
            let (k, key) = keys.first()?;
            let non_null = values.iter().filter(|v| v.is_some()).count();
            let has_null = values.iter().any(Option::is_none);
            let any = (non_null > 0).then(|| {
                let elems = vec![typed_null(key.type_oid); non_null];
                format!("{k} = ANY (ARRAY[{}])", elems.join(", "))
            });
            conjuncts.push(match (any, has_null) {
                (Some(any), false) => format!("{k} IS NOT NULL AND {any}"),
                (Some(any), true) => format!("({k} IS NULL OR {any})"),
                (None, _) => format!("{k} IS NULL"),
            });
        }
        Bound::Range { lower, upper } => {
            for (i, (k, key)) in keys.iter().enumerate() {
                conjuncts.push(format!("{k} IS NOT NULL"));
                if matches!(lower.get(i), Some(RangeDatum::Value(_))) {
                    conjuncts.push(format!("{k} >= {}", typed_null(key.type_oid)));
                }
                if matches!(upper.get(i), Some(RangeDatum::Value(_))) {
                    conjuncts.push(format!("{k} < {}", typed_null(key.type_oid)));
                }
            }
        }
    }
    let sql = format!("SELECT {}", conjuncts.join(" AND "));
    let parsed = typedpg_pg_query::parse(&sql).ok()?;
    let stmt = parsed.protobuf.stmts.into_iter().next()?.stmt?;
    let node::Node::SelectStmt(sel) = stmt.node? else {
        return None;
    };
    let node::Node::ResTarget(rt) = sel.target_list.into_iter().next()?.node? else {
        return None;
    };
    Some((*rt.val?, vars))
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

/// The partitions of `parent` in PartitionDesc order (partition_bounds_create):
/// range partitions by lower bound, list partitions by their smallest
/// non-NULL value (a NULL-only one after those), hash partitions by
/// (modulus, remainder), the default partition last. Bounds the analyzer
/// can't order (text values under a collation other than code point
/// order) come after the ordered ones, in creation order.
pub(crate) fn partition_desc_order(interp: &PgCatalog, parent: PgClassOid) -> Vec<PgClassOid> {
    // A total order: (group, orderable key, creation order).
    let key = |p: &PgClassOid| -> (u8, Option<Vec<(u8, Datum)>>, PgClassOid) {
        let orderable =
            |d: &Datum| matches!(d, Datum::Int(_) | Datum::Stamp(_) | Datum::CodePoints(_));
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
