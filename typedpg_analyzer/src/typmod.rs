//! Codec for `pg_attribute.atttypmod` / `pg_type.typtypmod`.
//!
//! PostgreSQL stores parametric type modifiers (the `n` in `varchar(n)`, the
//! `(p,s)` in `numeric(p,s)`, the dimension count in `vector(N)`, …) as a
//! single packed `int32`. The encoding depends on the type:
//!
//! | typname                                       | encoding                                  |
//! |-----------------------------------------------|-------------------------------------------|
//! | `varchar`, `bpchar`                           | `n + 4` (VARHDRSZ)                        |
//! | `numeric`                                     | `((p << 16) \| (s & 0x7FF)) + 4`          |
//! | `time`, `timetz`, `timestamp`, `timestamptz`  | `p` (precision, clamped to 0–6)           |
//! | `interval`                                    | `(fields << 16) \| p` (`p` = 0xFFFF: none) |
//! | `bit`, `varbit`                               | `n`                                       |
//! | `vector` (pgvector)                           | `n` (dimension)                           |
//!
//! `None` represents PG's `-1` sentinel ("no typmod").
//!
//! Functions in this module never panic on invalid AST input — they return a
//! `DdlError::UnsupportedDdl` so the caller can surface a clear migration
//! error.
//!
//! Decoding is mostly used for diagnostics (overflow messages) and to surface
//! structured info (precision, scale, dimension) to consumers.
//!
//! Behaviour when the type is not in the table above: a type PG knows has
//! no `typmodin` (the rest of `pg_catalog`, domains, enums, composites, …)
//! is rejected like PG does; a non-`pg_catalog` base type silently returns
//! `Ok(None)` so custom types with their own typmodin function don't break
//! the migration — we just don't track typmod for those.

use typedpg_pg_query::protobuf::{Node, TypeName, node};

use crate::ddl::DdlError;
use crate::error::AnalyzeError;
use crate::oid::PgTypeOid;
use crate::pg_catalog::{PgCatalog, oid as builtin_oid};

const VARHDRSZ: i32 = 4;
const MAX_NUMERIC_PRECISION: i32 = 1000;
pub(crate) const MAX_TIMESTAMP_PRECISION: i32 = 6;
const MAX_VECTOR_DIM: i32 = 16000;
/// `MaxAttrSize` (10 MB): the upper bound of a character / bit length.
const MAX_ATTR_SIZE: i32 = 10 * 1024 * 1024;
const BITS_PER_BYTE: i32 = 8;

/// Decoded view of a typmod, type-aware. `None` and the catch-all
/// `Other(i32)` keep the structure honest for types we don't model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodedTypmod {
    /// No typmod (PG's `-1`).
    None,
    /// Length-bounded string / bit type: `varchar(n)`, `bpchar(n)`, `bit(n)`.
    Length(i32),
    /// Decimal precision/scale: `numeric(p, s)`.
    Numeric { precision: i32, scale: i32 },
    /// Date/time precision: `timestamp(p)`, `time(p)`, `interval(p)`.
    Precision(i32),
    /// pgvector dimension count.
    VectorDim(i32),
    /// Recognised type whose typmod we don't decode further (or one whose
    /// shape isn't in the table above).
    Other(i32),
}

/// Encode the typmods a [`TypeName`] carries into the packed `i32` PG would
/// store — the analyzer's port of the type's `typmodin` function, reached
/// like PG's `typenameTypeMod`.
///
/// `Ok(None)` means either (a) no typmods supplied (`varchar` plain), (b) a
/// modifier that PG itself reduces to `-1` (`interval` over the full
/// range), or (c) a non-`pg_catalog` base type whose `typmodin` we don't
/// model. `Ok(Some(v))` is the encoded value. `Err` is for inputs PG
/// rejects: a type without a `typmodin` (`type modifier is not allowed for
/// type "text"`), a modifier that is no simple constant or identifier, one
/// that is no integer, the wrong modifier count, or an out-of-range value.
/// An array type uses its element's `typmodin` (so `varchar(5)[]` is 9).
pub fn encode(
    snapshot: &PgCatalog,
    type_oid: PgTypeOid,
    type_name: &TypeName,
) -> Result<Option<i32>, DdlError> {
    let typmods = &type_name.typmods;
    if typmods.is_empty() {
        return Ok(None);
    }
    let Some(t) = snapshot.get_type(type_oid) else {
        return Ok(None);
    };
    if t.typcategory == crate::pg_catalog::TypCategory::Array
        && let Some(elem) = t.typelem
        && snapshot.array_type_of(elem) == Some(type_oid)
    {
        return encode(snapshot, elem, type_name);
    }

    // `ArrayGetIntegerTypmods`, which every modeled `typmodin` starts with.
    let raw = || -> Result<Vec<i32>, DdlError> {
        typmod_strings(typmods)?
            .iter()
            .map(|s| typmod_integer(s))
            .collect()
    };

    if is_pgvector_type(snapshot, type_oid) {
        return encode_vector(&raw()?).map(Some);
    }

    let in_pg_catalog = snapshot.namespace_name(t.typnamespace) == Some("pg_catalog");
    let typname = t.typname.as_str();
    match (type_oid, in_pg_catalog.then_some(typname)) {
        // `anychar_typmodin` names bpchar `char` in its messages.
        (builtin_oid::VARCHAR, _) => {
            encode_length(&raw()?, "varchar", MAX_ATTR_SIZE, VARHDRSZ).map(Some)
        }
        (builtin_oid::BPCHAR, _) => {
            encode_length(&raw()?, "char", MAX_ATTR_SIZE, VARHDRSZ).map(Some)
        }
        (builtin_oid::NUMERIC, _) => encode_numeric(&raw()?).map(Some),
        (_, Some("time")) => encode_precision(&raw()?, "TIME", "").map(Some),
        (_, Some("timetz")) => encode_precision(&raw()?, "TIME", " WITH TIME ZONE").map(Some),
        (_, Some("timestamp")) => encode_precision(&raw()?, "TIMESTAMP", "").map(Some),
        (_, Some("timestamptz")) => {
            encode_precision(&raw()?, "TIMESTAMP", " WITH TIME ZONE").map(Some)
        }
        (_, Some("interval")) => encode_interval(&raw()?),
        // `anybit_typmodin`: the length itself, no VARHDRSZ.
        (_, Some(name @ ("bit" | "varbit"))) => {
            encode_length(&raw()?, name, MAX_ATTR_SIZE * BITS_PER_BYTE, 0).map(Some)
        }
        // A non-pg_catalog base type (extension / user C type) may have a
        // typmodin we don't model — drop the typmod rather than reject,
        // once `typenameTypeMod` has turned the modifiers into strings.
        (_, None) if t.typtype == crate::pg_catalog::TypType::Base => {
            typmod_strings(typmods)?;
            Ok(None)
        }
        // Everything else has no typmodin: pg_catalog types outside the
        // list above, domains, enums, composites, ranges, pseudo-types.
        // `typenameTypeMod` names the type as written (`TypeNameToString`).
        _ => Err(DdlError::UnsupportedDdl(format!(
            "type modifier is not allowed for type \"{}\"",
            crate::ddl::util::type_name_to_string(type_name)
        ))),
    }
}

/// Decode a packed typmod into a structured form. Returns `DecodedTypmod::None`
/// for `None` input.
pub fn decode(snapshot: &PgCatalog, type_oid: PgTypeOid, typmod: Option<i32>) -> DecodedTypmod {
    let Some(t) = typmod else {
        return DecodedTypmod::None;
    };

    if is_pgvector_type(snapshot, type_oid) {
        return DecodedTypmod::VectorDim(t);
    }

    let typname = snapshot.get_type(type_oid).map(|x| x.typname.as_str());
    match (type_oid, typname) {
        (builtin_oid::VARCHAR | builtin_oid::BPCHAR, _) => DecodedTypmod::Length(t - VARHDRSZ),
        (builtin_oid::NUMERIC, _) => {
            let inner = t - VARHDRSZ;
            let precision = (inner >> 16) & 0xFFFF;
            // numeric_typmod_scale: the scale is signed (PG allows a
            // negative one), an 11-bit field sign-extended.
            let scale = ((inner & 0x7FF) ^ 1024) - 1024;
            DecodedTypmod::Numeric { precision, scale }
        }
        (_, Some("time" | "timetz" | "timestamp" | "timestamptz")) => DecodedTypmod::Precision(t),
        // INTERVAL_TYPMOD(precision, range): the low 16 bits are the
        // precision (0xFFFF when only the field range is restricted).
        (_, Some("interval")) if t & 0xFFFF != INTERVAL_FULL_PRECISION => {
            DecodedTypmod::Precision(t & 0xFFFF)
        }
        (_, Some("bit" | "varbit")) => DecodedTypmod::Length(t),
        _ => DecodedTypmod::Other(t),
    }
}

// ─── Per-type encoders ─────────────────────────────────────────────────────

/// `anychar_typmodin` / `anybit_typmodin`: exactly one length in
/// `1..=max`, stored as `n + header`.
fn encode_length(raw: &[i32], kind: &str, max: i32, header: i32) -> Result<i32, DdlError> {
    let [n] = raw else {
        return Err(DdlError::UnsupportedDdl("invalid type modifier".into()));
    };
    let n = *n;
    if n < 1 {
        return Err(DdlError::UnsupportedDdl(format!(
            "length for type {kind} must be at least 1 (got {n})"
        )));
    }
    if n > max {
        return Err(DdlError::UnsupportedDdl(format!(
            "length for type {kind} cannot exceed {max}"
        )));
    }
    Ok(n + header)
}

/// `numerictypmodin`: `(p)` or `(p, s)` with `p` in 1..=1000 and (PG 15+)
/// `s` in -1000..=1000 — the scale may exceed the precision.
fn encode_numeric(raw: &[i32]) -> Result<i32, DdlError> {
    let (precision, scale) = match raw {
        [p] => (*p, 0),
        [p, s] => (*p, *s),
        _ => {
            return Err(DdlError::UnsupportedDdl(
                "invalid NUMERIC type modifier".into(),
            ));
        }
    };
    if !(1..=MAX_NUMERIC_PRECISION).contains(&precision) {
        return Err(DdlError::UnsupportedDdl(format!(
            "NUMERIC precision {precision} must be between 1 and {MAX_NUMERIC_PRECISION}"
        )));
    }
    if !(-MAX_NUMERIC_PRECISION..=MAX_NUMERIC_PRECISION).contains(&scale) {
        return Err(DdlError::UnsupportedDdl(format!(
            "NUMERIC scale {scale} must be between {} and {MAX_NUMERIC_PRECISION}",
            -MAX_NUMERIC_PRECISION
        )));
    }
    // make_numeric_typmod: the scale is an 11-bit two's-complement field.
    Ok(((precision << 16) | (scale & 0x7FF)) + VARHDRSZ)
}

/// `anytime_typmod_check` / `anytimestamp_typmod_check`: one non-negative
/// precision; above 6 PG only warns (`… precision reduced to maximum
/// allowed, 6`) and clamps.
fn encode_precision(raw: &[i32], label: &str, suffix: &str) -> Result<i32, DdlError> {
    let [p] = raw else {
        return Err(DdlError::UnsupportedDdl("invalid type modifier".into()));
    };
    clamp_time_precision(*p, label, suffix)
}

/// The shared precision rule of the time family, also applied to the
/// `CURRENT_TIME(p)` / `LOCALTIMESTAMP(p)` value functions.
pub(crate) fn clamp_time_precision(p: i32, label: &str, suffix: &str) -> Result<i32, DdlError> {
    if p < 0 {
        return Err(DdlError::UnsupportedDdl(format!(
            "{label}({p}){suffix} precision must not be negative"
        )));
    }
    Ok(p.min(MAX_TIMESTAMP_PRECISION))
}

// `INTERVAL_MASK` bits (datetime.h) and the typmod packing of timestamp.h.
const INTERVAL_MONTH: i32 = 1 << 1;
const INTERVAL_YEAR: i32 = 1 << 2;
const INTERVAL_DAY: i32 = 1 << 3;
const INTERVAL_HOUR: i32 = 1 << 10;
const INTERVAL_MINUTE: i32 = 1 << 11;
const INTERVAL_SECOND: i32 = 1 << 12;
const INTERVAL_FULL_RANGE: i32 = 0x7FFF;
const INTERVAL_FULL_PRECISION: i32 = 0xFFFF;

/// `INTERVAL_TYPMOD(p, r)`.
fn interval_typmod(precision: i32, range: i32) -> i32 {
    (range << 16) | (precision & 0xFFFF)
}

/// `intervaltypmodin`: the grammar hands over `[fields]` or
/// `[fields, precision]` (`interval(3)` is `[FULL_RANGE, 3]`, `interval
/// day` is `[DAY]`, `interval day to second(2)` is `[DAY|…|SECOND, 2]`).
/// The field mask must be one of the SQL-standard ranges; an unqualified
/// `interval` over the full range with no precision is `-1`.
fn encode_interval(raw: &[i32]) -> Result<Option<i32>, DdlError> {
    let invalid = || DdlError::UnsupportedDdl("invalid INTERVAL type modifier".into());
    let range = *raw.first().ok_or_else(invalid)?;
    const VALID: [i32; 14] = [
        INTERVAL_YEAR,
        INTERVAL_MONTH,
        INTERVAL_DAY,
        INTERVAL_HOUR,
        INTERVAL_MINUTE,
        INTERVAL_SECOND,
        INTERVAL_YEAR | INTERVAL_MONTH,
        INTERVAL_DAY | INTERVAL_HOUR,
        INTERVAL_DAY | INTERVAL_HOUR | INTERVAL_MINUTE,
        INTERVAL_DAY | INTERVAL_HOUR | INTERVAL_MINUTE | INTERVAL_SECOND,
        INTERVAL_HOUR | INTERVAL_MINUTE,
        INTERVAL_HOUR | INTERVAL_MINUTE | INTERVAL_SECOND,
        INTERVAL_MINUTE | INTERVAL_SECOND,
        INTERVAL_FULL_RANGE,
    ];
    if !VALID.contains(&range) {
        return Err(invalid());
    }
    match raw {
        [_] if range == INTERVAL_FULL_RANGE => Ok(None),
        [_] => Ok(Some(interval_typmod(INTERVAL_FULL_PRECISION, range))),
        [_, p] => {
            if *p < 0 {
                return Err(DdlError::UnsupportedDdl(format!(
                    "INTERVAL({p}) precision must not be negative"
                )));
            }
            // Above the maximum PG warns and clamps.
            Ok(Some(interval_typmod(
                (*p).min(MAX_TIMESTAMP_PRECISION),
                range,
            )))
        }
        _ => Err(invalid()),
    }
}

fn encode_vector(raw: &[i32]) -> Result<i32, DdlError> {
    if raw.len() != 1 {
        return Err(DdlError::UnsupportedDdl(format!(
            "vector type takes exactly one dimension argument, got {}",
            raw.len()
        )));
    }
    let n = raw[0];
    if !(1..=MAX_VECTOR_DIM).contains(&n) {
        return Err(DdlError::UnsupportedDdl(format!(
            "vector dimension {n} must be between 1 and {MAX_VECTOR_DIM}"
        )));
    }
    Ok(n)
}

// ─── Helpers ──────────────────────────────────────────────────────────────

/// `typenameTypeMod`'s modifiers as the strings handed to `typmodin`: an
/// integer, a numeric or string literal as written, or a bare identifier.
fn typmod_strings(typmods: &[Node]) -> Result<Vec<String>, DdlError> {
    use typedpg_pg_query::protobuf::a_const::Val;
    typmods
        .iter()
        .map(|tm| match tm.node.as_ref() {
            Some(node::Node::AConst(c)) => match c.val.as_ref() {
                Some(Val::Ival(i)) => Some(i.ival.to_string()),
                Some(Val::Fval(f)) => Some(f.fval.clone()),
                Some(Val::Sval(s)) => Some(s.sval.clone()),
                _ => None,
            },
            Some(node::Node::ColumnRef(cr)) => match cr.fields.as_slice() {
                [f] => match f.node.as_ref() {
                    Some(node::Node::String(s)) => Some(s.sval.clone()),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .map(|s| {
            s.ok_or_else(|| {
                DdlError::UnsupportedDdl(
                    "type modifiers must be simple constants or identifiers".into(),
                )
            })
        })
        .collect()
}

/// `ArrayGetIntegerTypmods`' `pg_strtoint32` of one modifier.
fn typmod_integer(s: &str) -> Result<i32, DdlError> {
    crate::literal_input::validate_int(s, i32::MIN.into(), i32::MAX.into(), "integer")
        .map_err(DdlError::Parse)?;
    crate::literal_input::parse_pg_integer(s)
        .and_then(|v| i32::try_from(v).ok())
        .ok_or_else(|| DdlError::Internal(format!("typmod {s:?} validated but unparsed")))
}

/// True when `oid` resolves to pgvector's `vector` type (any namespace,
/// since pgvector typically lives in `public` but users can install it
/// elsewhere).
fn is_pgvector_type(snapshot: &PgCatalog, oid: PgTypeOid) -> bool {
    snapshot
        .get_type(oid)
        .map(|t| t.typname == "vector")
        .unwrap_or(false)
}

// ─── Validation: literal vs column typmod ─────────────────────────────────

/// Check whether assigning `value` (a SQL literal node) to a column with
/// `(type_oid, typmod)` would violate the typmod's bound. Returns
/// `Some(error)` when we can prove a violation at compile time; `None`
/// otherwise (param refs, expressions, or types we don't validate).
pub fn check_literal_assignment(
    snapshot: &PgCatalog,
    type_oid: PgTypeOid,
    typmod: Option<i32>,
    value: &Node,
) -> Option<AnalyzeError> {
    let decoded = decode(snapshot, type_oid, typmod);
    match decoded {
        DecodedTypmod::Length(_)
            if matches!(type_oid, builtin_oid::VARCHAR | builtin_oid::BPCHAR) =>
        {
            let s = string_literal(value)?;
            char_length_violation(snapshot, type_oid, typmod, s).map(AnalyzeError::Invalid)
        }
        DecodedTypmod::Numeric { precision, scale } => {
            let raw = numeric_literal_string(value)?;
            // Content first, magnitude second — PG runs numeric_in before
            // applying the typmod, so a malformed literal is 22P02
            // (`invalid input syntax`), never `numeric field overflow`.
            if let Err(msg) = crate::literal_input::validate_numeric(&raw) {
                return Some(AnalyzeError::InvalidLiteral(msg));
            }
            numeric_overflow(&raw, precision, scale).map(AnalyzeError::Invalid)
        }
        DecodedTypmod::VectorDim(n) => {
            let count = vector_literal_dim_count(value)?;
            if count != n as usize {
                Some(AnalyzeError::Invalid(format!(
                    "expected {n} dimensions, not {count}"
                )))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Whether a column of type `type_oid` (a base type) with typmod `typmod`
/// stores constant `lit` exactly as written: no typmod, a `varchar(n)`
/// string of at most `n` characters (a longer one loses its trailing
/// spaces), or an integer in a `numeric(p, s)` of scale `s ≥ 0` (which
/// fits unchanged, or fails). Any other typmod coercion may truncate,
/// round or pad it (`numeric(2,-1)` makes 15 20, `timestamp(0)` drops
/// fractional seconds, `char(n)` pads).
pub(crate) fn keeps_literal(
    snapshot: &PgCatalog,
    type_oid: PgTypeOid,
    typmod: Option<i32>,
    lit: &crate::nonnull::Literal,
) -> bool {
    use crate::nonnull::LitKind;
    match decode(snapshot, type_oid, typmod) {
        DecodedTypmod::None => true,
        DecodedTypmod::Length(n) if type_oid == builtin_oid::VARCHAR => {
            lit.kind != LitKind::Boolean
                && usize::try_from(n).is_ok_and(|n| lit.text.chars().count() <= n)
        }
        DecodedTypmod::Numeric { scale, .. } => lit.kind == LitKind::Integer && scale >= 0,
        _ => false,
    }
}

/// `varchar_input` / `bpchar_input`: a `varchar(n)` / `char(n)` value
/// longer than `n` characters is an error unless every character past the
/// `n`th is a space (those are dropped). `None` for other types or a value
/// that fits.
pub(crate) fn char_length_violation(
    snapshot: &PgCatalog,
    type_oid: PgTypeOid,
    typmod: Option<i32>,
    s: &str,
) -> Option<String> {
    let DecodedTypmod::Length(n) = decode(snapshot, type_oid, typmod) else {
        return None;
    };
    // Match PG's wording: SQL-standard names instead of the catalog
    // `typname` so error messages line up across tooling (e.g. `\d+`
    // output / sqlstate-driven UIs).
    let typ_label = match type_oid {
        builtin_oid::VARCHAR => "character varying",
        builtin_oid::BPCHAR => "character",
        _ => return None,
    };
    let excess_not_blank = s
        .chars()
        .skip(usize::try_from(n).unwrap_or(0))
        .any(|c| c != ' ');
    excess_not_blank.then(|| format!("value too long for type {typ_label}({n})"))
}

fn string_literal(node: &Node) -> Option<&str> {
    match node.node.as_ref()? {
        node::Node::AConst(c) => match c.val.as_ref()? {
            typedpg_pg_query::protobuf::a_const::Val::Sval(s) => Some(s.sval.as_str()),
            _ => None,
        },
        // Allow `'abc'::varchar(N)` — drill through the cast so the literal
        // length still gets checked against the assignment target.
        node::Node::TypeCast(tc) => tc.arg.as_deref().and_then(string_literal),
        _ => None,
    }
}

fn numeric_literal_string(node: &Node) -> Option<String> {
    match node.node.as_ref()? {
        node::Node::AConst(c) => match c.val.as_ref()? {
            typedpg_pg_query::protobuf::a_const::Val::Ival(i) => Some(i.ival.to_string()),
            typedpg_pg_query::protobuf::a_const::Val::Fval(f) => Some(f.fval.clone()),
            typedpg_pg_query::protobuf::a_const::Val::Sval(s) => Some(s.sval.clone()),
            _ => None,
        },
        node::Node::TypeCast(tc) => tc.arg.as_deref().and_then(numeric_literal_string),
        _ => None,
    }
}

/// The error `numeric(p, s)` raises for the numeric literal `literal` (an
/// untyped string, an integer or a decimal constant), if it provably does:
/// a cast or assignment to a numeric typmod runs `apply_typmod` on the
/// value. `None` for any other node, type or typmod, or a value that fits
/// or doesn't parse (`numeric_in` reports that one).
pub(crate) fn numeric_literal_overflow(
    snapshot: &PgCatalog,
    type_oid: PgTypeOid,
    typmod: Option<i32>,
    literal: &Node,
) -> Option<String> {
    let DecodedTypmod::Numeric { precision, scale } = decode(snapshot, type_oid, typmod) else {
        return None;
    };
    let raw = match literal.node.as_ref()? {
        node::Node::AConst(c) if !c.isnull => match c.val.as_ref()? {
            typedpg_pg_query::protobuf::a_const::Val::Ival(i) => i.ival.to_string(),
            typedpg_pg_query::protobuf::a_const::Val::Fval(f) => f.fval.clone(),
            typedpg_pg_query::protobuf::a_const::Val::Sval(s) => s.sval.clone(),
            _ => return None,
        },
        _ => return None,
    };
    crate::literal_input::validate_numeric(&raw).ok()?;
    numeric_overflow(&raw, precision, scale)
}

/// `apply_typmod` / `apply_typmod_special` (numeric.c) on the valid
/// `numeric_in` input `raw`: the value is rounded (half away from zero) to
/// `scale` fractional digits, then must have at most `precision - scale`
/// integer digits; infinity never fits, NaN always does. `Some(message)`
/// with PG's `numeric field overflow` and its detail when it doesn't fit.
fn numeric_overflow(raw: &str, precision: i32, scale: i32) -> Option<String> {
    let overflow = |detail: String| {
        Some(format!(
            "numeric field overflow: a field with precision {precision}, scale {scale} {detail}"
        ))
    };
    let s = raw.trim_matches(|c: char| c.is_ascii_whitespace());
    let s = s.strip_prefix(['+', '-']).unwrap_or(s);
    match s.to_ascii_lowercase().as_str() {
        "nan" => return None,
        "inf" | "infinity" => return overflow("cannot hold an infinite value".into()),
        _ => {}
    }
    // The value as `digits × 10^exp`.
    let bytes = s.as_bytes();
    let (digits, exp): (String, i64) =
        if bytes.len() >= 2 && bytes[0] == b'0' && matches!(bytes[1] | 0x20, b'x' | b'o' | b'b') {
            let radix = match bytes[1] | 0x20 {
                b'x' => 16,
                b'o' => 8,
                _ => 2,
            };
            let mut value: u128 = 0;
            for c in s[2..].chars().filter(|&c| c != '_') {
                value = value
                    .checked_mul(radix)?
                    .checked_add(u128::from(c.to_digit(radix as u32)?))?;
            }
            (value.to_string(), 0)
        } else {
            let (mantissa, exponent) = match s.find(['e', 'E']) {
                Some(i) => (&s[..i], s[i + 1..].replace('_', "").parse::<i64>().ok()?),
                None => (s, 0),
            };
            let mantissa = mantissa.replace('_', "");
            let (int_part, frac) = mantissa.split_once('.').unwrap_or((&mantissa, ""));
            (
                format!("{int_part}{frac}"),
                exponent.checked_sub(i64::try_from(frac.len()).ok()?)?,
            )
        };
    let mut digits = digits.trim_start_matches('0').to_owned();
    let mut exp = exp;
    // round_var(var, scale): drop the digits past `scale` fractional ones,
    // rounding half away from zero.
    if exp < -i64::from(scale) {
        let drop = usize::try_from(-i64::from(scale) - exp).ok()?;
        let round_up = drop <= digits.len() && digits.as_bytes()[digits.len() - drop] >= b'5';
        digits.truncate(digits.len().saturating_sub(drop));
        if round_up {
            // Add one to the kept digits (a decimal string).
            let mut carry = true;
            let mut out: Vec<u8> = digits.into_bytes();
            for d in out.iter_mut().rev() {
                if *d == b'9' {
                    *d = b'0';
                } else {
                    *d += 1;
                    carry = false;
                    break;
                }
            }
            if carry {
                out.insert(0, b'1');
            }
            digits = String::from_utf8(out).ok()?;
        }
        exp = -i64::from(scale);
    }
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        // A true zero always fits.
        return None;
    }
    // The number of integer digits (≤ 0 below 1).
    let int_digits = i64::try_from(digits.len()).ok()? + exp;
    let max_digits = precision - scale;
    if int_digits <= i64::from(max_digits) {
        return None;
    }
    // (PG displays 10^0 as 1.)
    let bound = if max_digits == 0 {
        "1".to_owned()
    } else {
        format!("10^{max_digits}")
    };
    overflow(format!("must round to an absolute value less than {bound}"))
}

fn vector_literal_dim_count(node: &Node) -> Option<usize> {
    match node.node.as_ref()? {
        // ARRAY[1, 2, 3]::vector
        node::Node::AArrayExpr(arr) => Some(arr.elements.len()),
        // '[1,2,3]'::vector
        node::Node::TypeCast(tc) => {
            // First try drilling into the cast argument as an array.
            if let Some(arg) = tc.arg.as_deref()
                && let count = vector_literal_dim_count(arg)
                && count.is_some()
            {
                return count;
            }
            // Or a string literal in pgvector's `[1,2,3]` syntax.
            let s = string_literal(tc.arg.as_deref()?)?;
            parse_vector_string(s)
        }
        node::Node::AConst(_) => {
            let s = string_literal(node)?;
            parse_vector_string(s)
        }
        _ => None,
    }
}

fn parse_vector_string(s: &str) -> Option<usize> {
    let trimmed = s.trim();
    let inner = trimmed.strip_prefix('[')?.strip_suffix(']')?;
    if inner.trim().is_empty() {
        return Some(0);
    }
    Some(inner.split(',').count())
}
