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
        /// `string_to_array(s, ',')`. `None` when nothing is known — always
        /// the case for a table's array column (PostgreSQL has no NOT NULL
        /// for elements), a parameter, or an array that came through a
        /// view or most expressions (a subquery or CTE keeps what its
        /// query knows).
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
