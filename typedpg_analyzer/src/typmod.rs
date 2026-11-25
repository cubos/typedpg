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
//! | `geometry`, `geography` (PostGIS)             | `srid << 8 \| type << 2 \| z << 1 \| m`    |
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

use typedpg_pg_query::ast::{self, Node, TypeName};

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
    /// PostGIS's `geometry` / `geography` modifier: the geometry type
    /// (`TYPMOD_GET_TYPE`, 0 for any), dimensions and SRID (0 for unknown).
    Spatial {
        geometry_type: i32,
        z: bool,
        m: bool,
        srid: i32,
    },
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
    // typenameTypeMod: a shell type has no typmodin yet.
    if !t.typisdefined {
        return Err(DdlError::Parse(format!(
            "type modifier cannot be specified for shell type \"{}\"",
            crate::ddl::util::type_name_to_string(type_name)
        )));
    }
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

    if let Some(max) = pgvector_max_dimensions(snapshot, type_oid) {
        return encode_vector(&raw()?, &t.typname, max).map(Some);
    }
    if let Some(geography) = postgis_type(snapshot, type_oid) {
        return encode_spatial(&typmod_strings(typmods)?, geography).map(Some);
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
        // A non-pg_catalog base type created with a typmodin, which the
        // analyzer doesn't run — drop the typmod rather than reject, once
        // `typenameTypeMod` has turned the modifiers into strings.
        (_, None)
            if t.typtype == crate::pg_catalog::TypType::Base
                && snapshot.types_with_typmodin.contains(&type_oid) =>
        {
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

    if pgvector_max_dimensions(snapshot, type_oid).is_some()
        && snapshot
            .get_type(type_oid)
            .is_some_and(|t| t.typname == "vector")
    {
        return DecodedTypmod::VectorDim(t);
    }

    if postgis_type(snapshot, type_oid).is_some() {
        return DecodedTypmod::Spatial {
            geometry_type: (t & 0xFC) >> 2,
            z: t & 0x02 != 0,
            m: t & 0x01 != 0,
            // TYPMOD_GET_SRID: the 21-bit field, sign-extended.
            srid: ((t & 0x0FFF_FF00) - (t & 0x1000_0000)) >> 8,
        };
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

/// pgvector's `vector_typmod_in` / `halfvec_typmod_in` /
/// `sparsevec_typmod_in`: one dimension count, from 1 to the type's
/// maximum, which is the typmod.
fn encode_vector(raw: &[i32], typname: &str, max: i32) -> Result<i32, DdlError> {
    let [n] = raw else {
        return Err(DdlError::UnsupportedDdl("invalid type modifier".into()));
    };
    if *n < 1 {
        return Err(DdlError::UnsupportedDdl(format!(
            "dimensions for type {typname} must be at least 1"
        )));
    }
    if *n > max {
        return Err(DdlError::UnsupportedDdl(format!(
            "dimensions for type {typname} cannot exceed {max}"
        )));
    }
    Ok(*n)
}

/// liblwgeom's geometry type names (`geomtype_struct_array`, matched
/// upper-cased) and their numbers (`POINTTYPE` …), `GEOMETRY` being 0; each
/// takes a `Z`, `M` or `ZM` suffix.
const POSTGIS_GEOMETRY_TYPES: &[(&str, i32)] = &[
    ("GEOMETRY", 0),
    ("POINT", 1),
    ("LINESTRING", 2),
    ("POLYGON", 3),
    ("MULTIPOINT", 4),
    ("MULTILINESTRING", 5),
    ("MULTIPOLYGON", 6),
    ("GEOMETRYCOLLECTION", 7),
    ("CIRCULARSTRING", 8),
    ("COMPOUNDCURVE", 9),
    ("CURVEPOLYGON", 10),
    ("MULTICURVE", 11),
    ("MULTISURFACE", 12),
    ("POLYHEDRALSURFACE", 13),
    ("TRIANGLE", 14),
    ("TIN", 15),
];

/// `lwtype_name`: how `postgis_typmod_out` names a geometry type.
const POSTGIS_TYPE_NAMES: &[&str] = &[
    "Unknown",
    "Point",
    "LineString",
    "Polygon",
    "MultiPoint",
    "MultiLineString",
    "MultiPolygon",
    "GeometryCollection",
    "CircularString",
    "CompoundCurve",
    "CurvePolygon",
    "MultiCurve",
    "MultiSurface",
    "PolyhedralSurface",
    "Triangle",
    "Tin",
];

/// `SRID_DEFAULT`, the SRID a geography without one gets (WGS 84).
const SRID_DEFAULT: i32 = 4326;
/// `SRID_MAXIMUM` / `SRID_USER_MAXIMUM`.
const SRID_MAXIMUM: i64 = 999_999;
const SRID_USER_MAXIMUM: i64 = 998_999;

/// `Some(is_geography)` when `oid` is PostGIS's `geometry` or `geography`
/// (by name, with a `typmod_in`, as pgvector's types are found).
fn postgis_type(snapshot: &PgCatalog, oid: PgTypeOid) -> Option<bool> {
    if !snapshot.types_with_typmodin.contains(&oid) {
        return None;
    }
    match snapshot.get_type(oid)?.typname.as_str() {
        "geometry" => Some(false),
        "geography" => Some(true),
        _ => None,
    }
}

/// PostGIS's `geometry_typmod_in` / `geography_typmod_in`
/// (gserialized_typmod.c, `gserialized_typmod_in`): a geometry type name,
/// then an SRID (clamped by `clamp_srid`); further modifiers are ignored.
/// A geography's SRID defaults to 4326 and must be a lon/lat one
/// (`srid_check_latlong`).
fn encode_spatial(raw: &[String], geography: bool) -> Result<i32, DdlError> {
    let mut srid: i64 = if geography { SRID_DEFAULT.into() } else { 0 };
    let mut typmod: i32 = 0;
    if let Some(name) = raw.first() {
        // geometry_type_from_string: trimmed of spaces, upper-cased.
        let upper = name.trim_matches(' ').to_ascii_uppercase();
        let (base, z, m) = if let Some(b) = upper.strip_suffix("ZM") {
            (b, true, true)
        } else if let Some(b) = upper.strip_suffix('Z') {
            (b, true, false)
        } else if let Some(b) = upper.strip_suffix('M') {
            (b, false, true)
        } else {
            (upper.as_str(), false, false)
        };
        // No base name ends in Z or M.
        let Some(&(_, number)) = POSTGIS_GEOMETRY_TYPES.iter().find(|(n, _)| *n == base) else {
            return Err(DdlError::UnsupportedDdl(format!(
                "Invalid geometry type modifier: {name}"
            )));
        };
        typmod = (number << 2) | (i32::from(z) << 1) | i32::from(m);
    }
    if let Some(text) = raw.get(1) {
        // strtol: leading white space, a sign and digits, nothing after.
        let digits = text.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
        let unsigned = digits.strip_prefix(['+', '-']).unwrap_or(digits);
        let invalid =
            || DdlError::Parse(format!("invalid input syntax for type integer: \"{text}\""));
        if unsigned.is_empty() || !unsigned.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid());
        }
        let value: i64 = digits
            .parse::<i64>()
            .ok()
            .filter(|v| i32::try_from(*v).is_ok())
            .ok_or_else(|| {
                DdlError::Parse(format!("value \"{text}\" is out of range for type integer"))
            })?;
        // clamp_srid (the NOTICEs it raises aren't errors).
        let clamped = if value <= 0 {
            0
        } else if value > SRID_MAXIMUM {
            SRID_USER_MAXIMUM + 1 + value % (SRID_MAXIMUM - SRID_USER_MAXIMUM - 1)
        } else {
            value
        };
        if clamped != 0 {
            srid = clamped;
        }
    }
    if geography && srid != i64::from(SRID_DEFAULT) && srid != 0 {
        check_latlong(srid)?;
    }
    // TYPMOD_SET_SRID: 21 bits from bit 8.
    let srid = i32::try_from(srid).unwrap_or(0);
    Ok(typmod | ((srid & 0x001F_FFFF) << 8))
}

/// `srid_check_latlong`: a geography's SRID must name a geographic
/// (lon/lat) coordinate system in `spatial_ref_sys`. The analyzer knows the
/// rows PostGIS installs: one there is geographic when PROJ reads it as
/// angular, which for every bundled row is a `+proj=longlat` definition
/// without vertical units (checked against PostGIS 3.7.0's 8500 rows). A
/// SRID missing from them may be one the migrations inserted: accepted.
///
/// The SRIDs from 999000 are PostGIS's own (lwgeom_transform.c's
/// `getProjDefinition`), never in the table: projected ones — world
/// mercator, the UTM, Lambert, stereographic and LAEA zones — and the rest
/// invalid (999283 is an LAEA zone of a band that doesn't exist).
fn check_latlong(srid: i64) -> Result<(), DdlError> {
    const RESERVE_OFFSET: i64 = 999_000;
    if srid >= RESERVE_OFFSET {
        return Err(DdlError::UnsupportedDdl(match srid {
            999_000..=999_062 | 999_101..=999_282 => {
                "Only lon/lat coordinate systems are supported in geography.".to_owned()
            }
            999_283 => "Unknown yzone encountered!".to_owned(),
            _ => format!("Invalid reserved SRID ({srid})"),
        }));
    }
    let Some(proj4text) = crate::ddl::extensions::postgis_proj4text(srid) else {
        return Ok(());
    };
    if proj4text.contains("+proj=longlat") && !proj4text.contains("+vunits") {
        return Ok(());
    }
    Err(DdlError::UnsupportedDdl(
        "Only lon/lat coordinate systems are supported in geography.".into(),
    ))
}

/// `postgis_typmod_out`: `(Type[Z][M][,srid])`, empty when the modifier
/// says nothing.
pub(crate) fn spatial_typmod_out(geometry_type: i32, z: bool, m: bool, srid: i32) -> String {
    if geometry_type == 0 && !z && !m && srid == 0 {
        return String::new();
    }
    let mut out = String::from("(");
    if geometry_type != 0 {
        out.push_str(
            POSTGIS_TYPE_NAMES
                .get(geometry_type as usize)
                .copied()
                .unwrap_or("Invalid type"),
        );
    } else {
        out.push_str("Geometry");
    }
    if z {
        out.push('Z');
    }
    if m {
        out.push('M');
    }
    if srid > 0 {
        out.push_str(&format!(",{srid}"));
    }
    out.push(')');
    out
}

// ─── Helpers ──────────────────────────────────────────────────────────────

/// `typenameTypeMod`'s modifiers as the strings handed to `typmodin`: an
/// integer, a numeric or string literal as written, or a bare identifier.
fn typmod_strings(typmods: &[Option<Node>]) -> Result<Vec<String>, DdlError> {
    use typedpg_pg_query::ast::AConstValue;
    typmods
        .iter()
        .map(|tm| match tm.as_ref() {
            Some(ast::Node::AConst(c)) => match c.val.as_ref() {
                Some(AConstValue::Integer(i)) => Some(i.ival.to_string()),
                Some(AConstValue::Float(f)) => Some(f.fval.clone()),
                Some(AConstValue::String(s)) => Some(s.sval.clone()),
                _ => None,
            },
            Some(ast::Node::ColumnRef(cr)) => match cr.fields.as_slice() {
                [f] => match f.as_ref() {
                    Some(ast::Node::String(s)) => Some(s.sval.clone()),
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
/// The most dimensions a pgvector type takes (`VECTOR_MAX_DIM`,
/// `HALFVEC_MAX_DIM`, `SPARSEVEC_MAX_DIM`), or `None` for another type.
fn pgvector_max_dimensions(snapshot: &PgCatalog, oid: PgTypeOid) -> Option<i32> {
    let t = snapshot.get_type(oid)?;
    if !snapshot.types_with_typmodin.contains(&oid) {
        return None;
    }
    match t.typname.as_str() {
        "vector" => Some(MAX_VECTOR_DIM),
        "halfvec" => Some(16_000),
        "sparsevec" => Some(1_000_000_000),
        _ => None,
    }
}

// ─── Validation: literal vs column typmod ─────────────────────────────────

/// Check whether assigning `value` (a SQL literal node) to a column with
/// `(type_oid, typmod)` would violate the typmod's bound or the type's
/// range. Returns `Some(error)` when we can prove a violation at compile
/// time; `None` otherwise (param refs, expressions, or types we don't
/// validate).
///
/// Every one of these errors comes from a coercion the planner folds — a
/// length coercion function (`varchar(varchar, int4, bool)`,
/// `numeric(numeric, int4)`), a conversion (`int2(int4)`) — not from
/// parse analysis (`coerce_type` reads an untyped literal with typmod
/// -1): it is reported only for a statement that is planned (see
/// [`crate::expr::with_plan_time_checks`]), not for one a function body
/// or a rule only stores.
pub fn check_literal_assignment(
    snapshot: &PgCatalog,
    type_oid: PgTypeOid,
    typmod: Option<i32>,
    value: Option<&Node>,
) -> Option<AnalyzeError> {
    assignment_error(
        snapshot,
        type_oid,
        typmod,
        value,
        crate::error::node_location(value),
        || crate::error::expr_span(value),
    )
}

/// [`check_literal_assignment`] of a value that may not be in the
/// statement's text (a column's default): a folding error points at
/// `span()`, and is about the node at `location` (see
/// [`crate::expr::plan_time_error`]).
pub(crate) fn assignment_error(
    snapshot: &PgCatalog,
    type_oid: PgTypeOid,
    typmod: Option<i32>,
    value: Option<&Node>,
    location: Option<i32>,
    span: impl FnOnce() -> Option<crate::error::SourceSpan>,
) -> Option<AnalyzeError> {
    if !crate::expr::plan_time_checks() {
        return None;
    }
    // A column of a subquery the planner pulled up is the subquery's
    // output expression by the time the conversion is folded.
    let substituted = crate::resolve::substituted(value);
    let value = substituted.as_deref().or(value);
    let e = assignment_error_here(snapshot, type_oid, typmod, value, span)?;
    crate::expr::plan_time_error(location, e)
}

/// [`check_literal_assignment`]'s error where the executor, not the
/// planner, converts the value: an output of a subquery that isn't pulled
/// up, assigned to its column once evaluated. Not a plan-time error, so
/// the caller decides whether the statement runs.
pub(crate) fn evaluated_assignment_error(
    snapshot: &PgCatalog,
    type_oid: PgTypeOid,
    typmod: Option<i32>,
    value: Option<&Node>,
) -> Option<AnalyzeError> {
    assignment_error_here(snapshot, type_oid, typmod, value, || {
        crate::error::expr_span(value)
    })
}

fn assignment_error_here(
    snapshot: &PgCatalog,
    type_oid: PgTypeOid,
    typmod: Option<i32>,
    value: Option<&Node>,
    span: impl FnOnce() -> Option<crate::error::SourceSpan>,
) -> Option<AnalyzeError> {
    if let Some(e) = crate::fold::conversion_error(value, type_oid, typmod, snapshot) {
        return Some(e.raw(span()).finalize_implicit());
    }
    // coerce_to_domain applies the base type's typmod (the domain's
    // typtypmod) with the base type's length coercion function, under the
    // same coercion context: a domain over `varchar(3)` fails like a
    // `varchar(3)` column.
    let base = snapshot.unwrap_domain(type_oid);
    let typmod = if base == type_oid {
        typmod
    } else {
        snapshot.effective_typmod(type_oid, None)
    };
    let type_oid = base;
    match decode(snapshot, type_oid, typmod) {
        DecodedTypmod::Length(_)
            if matches!(type_oid, builtin_oid::VARCHAR | builtin_oid::BPCHAR) =>
        {
            let s = assigned_string(value, snapshot)?;
            char_length_violation(snapshot, type_oid, typmod, s).map(AnalyzeError::Invalid)
        }
        DecodedTypmod::Length(n) if matches!(type_oid, BIT | VARBIT) => {
            let len = assigned_bit_length(value)?;
            bit_length_violation(type_oid == BIT, n, len).map(AnalyzeError::Invalid)
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

const BIT: PgTypeOid = PgTypeOid::from_raw(1560);
const VARBIT: PgTypeOid = PgTypeOid::from_raw(1562);

/// The length in bits of the bit string constant `node` assigns: a `B'…'`
/// / `X'…'` constant, or an untyped literal read by `bit_in` (a `b` / `x`
/// prefix selects binary / hexadecimal, four bits per hex digit). Not a
/// cast, which may have truncated or padded it already.
fn assigned_bit_length(node: Option<&Node>) -> Option<usize> {
    let ast::Node::AConst(c) = node? else {
        return None;
    };
    let text = match c.val.as_ref()? {
        typedpg_pg_query::ast::AConstValue::BitString(b) => b.bsval.as_str(),
        typedpg_pg_query::ast::AConstValue::String(s) => s.sval.as_str(),
        _ => return None,
    };
    crate::literal_input::validate_bit(text).ok()?;
    Some(match text.as_bytes().first() {
        Some(b'b' | b'B') => text.len() - 1,
        Some(b'x' | b'X') => (text.len() - 1) * 4,
        _ => text.len(),
    })
}

/// The implicit (assignment) length coercion of `bit(n)` / `varbit(n)`
/// (varbit.c's `bit` and `varbit` functions, `isExplicit` false): a
/// `bit(n)` value must be exactly `n` bits long, a `varbit(n)` one at most
/// `n`. (An explicit cast pads or truncates instead.)
fn bit_length_violation(fixed: bool, n: i32, len: usize) -> Option<String> {
    let n_len = usize::try_from(n).ok()?;
    if fixed && len != n_len {
        Some(format!(
            "bit string length {len} does not match type bit({n})"
        ))
    } else if !fixed && len > n_len {
        Some(format!("bit string too long for type bit varying({n})"))
    } else {
        None
    }
}

/// The string `node` assigns, as written: a string literal, or one cast
/// to a string type that keeps it whole (`'abc'::text`) — not to one
/// that may truncate it (`'abcd'::varchar(2)` is `'ab'`, `::char` is
/// `char(1)`).
fn assigned_string<'a>(node: Option<&'a Node>, snapshot: &PgCatalog) -> Option<&'a str> {
    match node? {
        ast::Node::AConst(c) => match c.val.as_ref()? {
            typedpg_pg_query::ast::AConstValue::String(s) => Some(s.sval.as_str()),
            _ => None,
        },
        ast::Node::TypeCast(tc) => {
            let tn = tc.type_name.as_ref()?;
            let keeps = tn.typmods.is_empty()
                && tn.array_bounds.is_empty()
                && crate::ddl::util::resolve_type_name(tn, snapshot).is_some_and(|t| {
                    matches!(
                        t,
                        builtin_oid::TEXT | builtin_oid::VARCHAR | builtin_oid::BPCHAR
                    )
                });
            if keeps {
                assigned_string(tc.arg.as_ref(), snapshot)
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

fn string_literal(node: Option<&Node>) -> Option<&str> {
    match node? {
        ast::Node::AConst(c) => match c.val.as_ref()? {
            typedpg_pg_query::ast::AConstValue::String(s) => Some(s.sval.as_str()),
            _ => None,
        },
        // Allow `'abc'::varchar(N)` — drill through the cast so the literal
        // length still gets checked against the assignment target.
        ast::Node::TypeCast(tc) => string_literal(tc.arg.as_ref()),
        _ => None,
    }
}

/// `apply_typmod` / `apply_typmod_special` (numeric.c) on the valid
/// `numeric_in` input `raw`: the value is rounded (half away from zero) to
/// `scale` fractional digits, then must have at most `precision - scale`
/// integer digits; infinity never fits, NaN always does. `Some(message)`
/// with PG's `numeric field overflow` and its detail when it doesn't fit.
pub(crate) fn numeric_overflow(raw: &str, precision: i32, scale: i32) -> Option<String> {
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

fn vector_literal_dim_count(node: Option<&Node>) -> Option<usize> {
    match node? {
        // ARRAY[1, 2, 3]::vector
        ast::Node::AArrayExpr(arr) => Some(arr.elements.len()),
        // '[1,2,3]'::vector
        ast::Node::TypeCast(tc) => {
            // First try drilling into the cast argument as an array.
            let count = vector_literal_dim_count(tc.arg.as_ref());
            if count.is_some() {
                return count;
            }
            // Or a string literal in pgvector's `[1,2,3]` syntax.
            let s = string_literal(tc.arg.as_ref())?;
            parse_vector_string(s)
        }
        ast::Node::AConst(_) => {
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
