//! Analyzer-facing representation of PostgreSQL types.
//!
//! The analyzer deals only with PG semantics: it does not know how a type
//! maps to Rust. Consumers (like the `typedpg_macros` crate) pattern-match
//! on this enum to decide the Rust target type.
//!
//! Nullability is *not* part of the outer type — it is a property of the
//! column / parameter site and is carried alongside on
//! [`crate::AnalyzedColumn`] / [`crate::AnalyzedParam`]. Inside an
//! `AnonymousRecord` the fields *do* carry per-element nullability because
//! every field is its own column-like site (`ROW(NOT_NULL_col, NULL_col)`
//! has one nullable element and one not). An `Array` carries what the
//! analysis knows about its elements' nullability the same way.

use typedpg_core::QualifiedName;

/// PostgreSQL's `format_type` spelling of a `pg_catalog` type, as messages
/// use it. Types without an alias keep their catalog name.
fn basic_name(name: &str) -> &str {
    match name {
        "bool" => "boolean",
        "char" => "\"char\"",
        "int2" => "smallint",
        "int4" => "integer",
        "int8" => "bigint",
        "float4" => "real",
        "float8" => "double precision",
        "bpchar" => "character",
        "varchar" => "character varying",
        "varbit" => "bit varying",
        "timestamp" => "timestamp without time zone",
        "timestamptz" => "timestamp with time zone",
        "time" => "time without time zone",
        "timetz" => "time with time zone",
        other => other,
    }
}

/// The `(…)` a typmod appends to a diagnostic's type name, when the type's
/// encoding is known here (see the `typmod` module).
fn typmod_suffix(name: &str, typmod: i32) -> Option<String> {
    match name {
        // `n + 4` (VARHDRSZ) for the length-bounded string types.
        "bpchar" | "varchar" => Some(format!("({})", typmod - 4)),
        // `((p << 16) | s) + 4` for numeric.
        "numeric" => Some(format!("({},{})", (typmod - 4) >> 16, (typmod - 4) & 0x7ff)),
        // The precision itself, for the date/time family.
        "time" | "timetz" | "timestamp" | "timestamptz" => Some(format!("({typmod})")),
        // `(fields << 16) | p`, the precision in the low half; 0xFFFF means
        // none.
        "interval" => {
            let precision = typmod & 0xffff;
            (precision != 0xffff).then(|| format!("({precision})"))
        }
        // The length itself.
        "bit" | "varbit" | "vector" => Some(format!("({typmod})")),
        _ => None,
    }
}

/// One field of an [`Type::AnonymousRecord`]. Carries the field's name, its
/// resolved [`Type`], and whether the value at that position can be NULL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordField {
    pub name: String,
    pub ty: Type,
    pub nullable: bool,
}

/// A resolved PostgreSQL type.
///
/// The seven variants cover everything PG's type system expresses that a
/// query analyzer can observe at static-analysis time:
///
/// - `Basic` — scalar PG types (`int4`, `text`, `uuid`, …) including pseudo
///   types (`void`, `record`, `anyelement`).
/// - `Domain` — `CREATE DOMAIN` wrappers. The base type is preserved so
///   consumers can either treat the domain opaquely or unwrap to the base.
/// - `Array` — PG array (`int4[]`). Multidimensional arrays share a type
///   with their one-dimensional form.
/// - `Enum` — `CREATE TYPE ... AS ENUM (...)` with the labels in declaration
///   order.
/// - `Range` — `CREATE TYPE ... AS RANGE (...)` / built-in range types.
/// - `Composite` — `CREATE TYPE ... AS (...)` and the implicit row type of
///   every table, carrying both its schema-qualified name (so it lines up
///   with PG's wire-protocol Describe OID) *and* the decomposed field list
///   (so the macro layer can synthesise per-field accessors without an
///   extra catalog round-trip). PG's composite types have no top-level
///   typmod or collation — both attributes live on the individual fields.
/// - `AnonymousRecord` — the unnamed row type produced by `ROW(...)` or a
///   subquery without a registered composite, carrying its named field
///   list with per-field nullability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    Basic {
        schema: String,
        name: String,
        extension: Option<String>,
        /// `pg_attribute.atttypmod`-style modifier. `Some(n + 4)` for
        /// `varchar(n)`, `Some(p)` for `timestamp(p)`, `Some(N)` for
        /// pgvector's `vector(N)`, etc. `None` matches PG's `-1` ("no
        /// typmod"). Use [`crate::typmod::decode`] to interpret the value
        /// per type.
        typmod: Option<i32>,
        /// Collation name attached to this column/expression — only
        /// surfaced for text-like types whose collation isn't the
        /// database default. Mirrors the way PG decorates a column with
        /// a non-default collation in the row description.
        collation: Option<String>,
    },
    Domain {
        schema: String,
        name: String,
        base: Box<Type>,
        extension: Option<String>,
        /// Effective typmod observed at the domain level. Mirrors
        /// `pg_type.typtypmod` if the column inherited it from the domain,
        /// or carries the column's own `atttypmod` when the column locally
        /// pinned a length / precision.
        typmod: Option<i32>,
        /// Same shape as on `Basic`: non-default collation pinned on the
        /// column / expression.
        collation: Option<String>,
    },
    Array {
        element: Box<Type>,
        /// Whether the array's elements can be NULL, where the analysis
        /// knows: `Some(true)` for `array_agg(nullable_col)` or
        /// `ARRAY[a, NULL]`, `Some(false)` for `array_agg(not_null_col)` or
        /// `string_to_array(s, ',')`, or a table's column (or a domain) whose
        /// CHECK says `array_position(c, NULL) IS NULL`. `None` when nothing
        /// is known — the case for any other table's array column
        /// (PostgreSQL has no NOT NULL for elements), a parameter, or an
        /// array most expressions make (a subquery, CTE or view keeps what
        /// its query knows).
        element_nullable: Option<bool>,
    },
    Enum {
        schema: String,
        name: String,
        labels: Vec<String>,
        extension: Option<String>,
    },
    Range {
        schema: String,
        name: String,
        subtype: Box<Type>,
        extension: Option<String>,
        /// Range subtypes don't carry typmod themselves but inherit through
        /// `subtype` — kept for symmetry with `Basic`/`Domain` and forward
        /// compatibility with custom range types.
        typmod: Option<i32>,
        /// A multirange (`int4multirange`) rather than a range: a set of
        /// ranges over `subtype`, with its own binary format.
        multirange: bool,
    },
    Composite {
        schema: String,
        name: String,
        /// Field shape decomposed for the macro layer. The column types
        /// inside carry their own typmod / collation; the composite itself
        /// has neither (PG sets `pg_type.typtypmod = -1` and rejects
        /// `COLLATE` on composite types).
        fields: Vec<RecordField>,
        extension: Option<String>,
    },
    AnonymousRecord {
        fields: Vec<RecordField>,
    },
}

impl Type {
    /// How a diagnostic names the type: PostgreSQL's SQL spelling of the
    /// built-ins (`integer`, `character varying`, `timestamp with time
    /// zone`), the qualified name of a user type, `[]` after an array's
    /// element and `record` for an anonymous record. A typmod is included
    /// where the type carries one (`character varying(5)`, `numeric(10,2)`).
    pub fn describe(&self) -> String {
        match self {
            Type::Basic {
                schema,
                name,
                typmod,
                collation,
                ..
            } => {
                let mut out = if schema == "pg_catalog" {
                    basic_name(name).to_owned()
                } else {
                    QualifiedName::new(schema.clone(), name.clone()).to_string()
                };
                if let Some(typmod) = *typmod
                    && let Some(suffix) = typmod_suffix(name, typmod)
                {
                    out.push_str(&suffix);
                }
                if let Some(collation) = collation {
                    out.push_str(" COLLATE ");
                    out.push_str(&typedpg_core::quote_identifier(collation));
                }
                out
            }
            Type::Domain { schema, name, .. }
            | Type::Enum { schema, name, .. }
            | Type::Range { schema, name, .. }
            | Type::Composite { schema, name, .. } => {
                QualifiedName::new(schema.clone(), name.clone()).to_string()
            }
            Type::Array { element, .. } => format!("{}[]", element.describe()),
            Type::AnonymousRecord { .. } => "record".to_owned(),
        }
    }

    /// The schema-qualified PG name to use for an explicit cast
    /// (`::pg_catalog.jsonb`, `::public.vector`). Domains are unwrapped to
    /// their base type. Arrays return the canonical element name suffixed
    /// with `[]`. Anonymous records have no cast name.
    ///
    /// The name is rendered via [`QualifiedName`]'s `Display`, which quotes
    /// identifiers when needed — safe to interpolate into SQL.
    pub fn cast_name(&self) -> Option<String> {
        match self {
            Type::Basic { schema, name, .. }
            | Type::Enum { schema, name, .. }
            | Type::Range { schema, name, .. }
            | Type::Composite { schema, name, .. } => {
                Some(QualifiedName::new(schema.clone(), name.clone()).to_string())
            }
            Type::Domain { base, .. } => base.cast_name(),
            Type::Array { element, .. } => element.cast_name().map(|n| format!("{n}[]")),
            Type::AnonymousRecord { .. } => None,
        }
    }
}
