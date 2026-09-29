//! Static SQL type and nullability analyzer for `typedpg`.
//!
//! This crate does at compile time what a live PostgreSQL connection would do
//! at runtime: it understands a SQL template's parameter types, its output
//! columns, and the nullability of both — without requiring Docker or a
//! running database.
//!
//! # Public surface
//!
//! The entry point is [`PgCatalog`]. Everything else is either returned by
//! [`PgCatalog::analyze`] or used to configure it:
//!
//! | Item | Role |
//! |------|------|
//! | [`PgCatalog`] | Mutable catalog: seed the PG18 catalog, then apply DDL and analyze queries against it. |
//! | [`AnalyzedQuery`] | Result of analysis: rewritten SQL + typed parameters, spreads, and output columns. |
//! | [`AnalyzedParam`] | A named parameter with its inferred Rust type and the byte offsets where it appears. |
//! | [`AnalyzedSpread`] | A `$..name { ... }` spread with its insertion offset and typed fields. |
//! | [`AnalyzedSpreadField`] | A single field inside a spread. |
//! | [`AnalyzedColumn`] | A single output column: name, Rust type, nullability. |
//! | [`AnalyzeError`] | Errors returned by [`PgCatalog::analyze`]. |
//! | [`DdlError`] | Errors returned by [`PgCatalog::apply_sql`]. |
//!
//! # Typical flow
//!
//! ```ignore
//! let mut db = PgCatalog::new();
//! db.apply_sql("CREATE TABLE users (id bigint primary key, name text not null);")?;
//! let result = db.analyze("SELECT id, name FROM users WHERE id = $id")?;
//! // result.columns[0].rust_type == "i64"
//! // result.params[0].rust_type  == "i64"
//! ```

mod array_input;
mod builtin_nullability;
mod clause;
mod coerce;
mod datetime_input;
mod ddl;
mod diagnostic;
mod error;
mod expr;
mod functions;
mod grouping;
mod jsonpath_input;
mod lexer;
mod literal_input;
mod lookup;
mod network_input;
mod nullability;
mod oid;
mod param;
mod param_collector;
mod pg_catalog;
#[cfg(feature = "pg_sanity")]
mod pg_sanity;
mod pgmsg;
mod polymorphic;
#[cfg(feature = "pg_sanity")]
pub use pg_sanity::{Divergence, DivergenceKind};
mod range_input;
mod regex_input;
mod resolve;
mod scope;
mod seed;
mod suggest;
mod tsearch_input;
mod types;
mod typmod;
mod xml_input;

/// Re-exports of types defined in `typedpg_core` but used pervasively by
/// the analyzer. Kept here so downstream crates (and tests) can depend only
/// on `typedpg_analyzer`.
pub(crate) mod qualified_name {
    pub use typedpg_core::QualifiedName;
}

pub use oid::{
    PgCastOid, PgClassOid, PgCollationOid, PgConstraintOid, PgEnumOid, PgExtensionOid,
    PgGenericOid, PgNamespaceOid, PgOpclassOid, PgOperatorOid, PgProcOid, PgRewriteOid, PgTypeOid,
};
#[cfg(any(test, feature = "internal"))]
pub use pg_catalog::{
    AggKind, ArgMode, AstBinding, AttGenerated, AttIdentity, CastContext, CastMethod, ConType,
    DepType, EvEnabled, EvType, PgAggregate, PgAm, PgAmop, PgAttribute, PgCast, PgCatalogSeed,
    PgClass, PgCollation, PgConstraint, PgDepend, PgEnum, PgExtension, PgIndex, PgInherits,
    PgNamespace, PgOpclass, PgOperator, PgOpfamily, PgProc, PgRange, PgRewrite, PgSetting,
    PgTsObject, PgType, ProKind, ProVolatile, RelKind, SerializedAst, TypAlign, TypCategory,
    TypStorage, TypType,
};

pub use ddl::DdlError;
pub use error::AnalyzeError;
pub use pg_catalog::PgCatalog;
pub use resolve::{
    AnalyzedColumn, AnalyzedParam, AnalyzedQuery, AnalyzedSpread, AnalyzedSpreadField,
};
pub use typedpg_core::{ParseQualifiedNameError, QualifiedName};
pub use types::{RecordField, Type};
