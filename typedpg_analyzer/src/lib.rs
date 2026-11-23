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
//! | [`PgCatalog`] | Mutable catalog: seed the PG19 catalog, then apply DDL and analyze queries against it. |
//! | [`QueryView`] | A query's analyzed text, parse tree and offset map, before the simplifications. |
//! | [`SublinkNullability`] | What the analyzer proved about the two sides of an `= ANY` / `IN` sublink. |
//! | [`MigrationObserver`] | Watches each statement of a migration as [`PgCatalog::apply_migration_observed`] applies it. |
//! | [`AnalyzedQuery`] | Result of analysis: rewritten SQL + typed parameters, spreads, and output columns. |
//! | [`AnalyzedParam`] | A named parameter with its inferred Rust type and the byte offsets where it appears. |
//! | [`AnalyzedSpread`] | A `$..name { ... }` spread with its insertion offset and typed fields. |
//! | [`AnalyzedSpreadField`] | A single field inside a spread. |
//! | [`AnalyzedColumn`] | A single output column: name, Rust type, nullability. |
//! | [`AnalysisDifference`] | The first difference between two analyses of the same statement, as a diagnostic. |
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

mod analysis_diff;
mod array_input;
mod builtin_nullability;
mod clause;
mod coerce;
mod condition;
mod datetime_input;
mod ddl;
mod decimal;
mod diagnostic;
mod error;
mod expr;
mod fold;
mod functions;
mod grouping;
mod having;
mod jsonpath_input;
mod lexer;
mod literal_input;
mod lookup;
mod network_input;
mod node_key;
mod nonnull;
mod nullability;
mod oid;
mod param;
mod param_collector;
mod pg_catalog;
#[cfg(feature = "pg_sanity")]
mod pg_sanity;
mod pgmsg;
mod polymorphic;
mod privileges;
#[cfg(feature = "pg_sanity")]
pub use pg_sanity::{Divergence, DivergenceKind};
mod range_input;
mod refine;
mod refine_bounds;
mod reg_input;
mod regex_input;
mod resolve;
mod scope;
mod seed;
mod simplify;
mod sql_text;
mod stack;
mod suggest;
mod template;
mod tsearch_input;
mod type_facts;
mod types;
mod typmod;
mod xml_input;

/// Re-exports of types defined in `typedpg_core` but used pervasively by
/// the analyzer. Kept here so downstream crates (and tests) can depend only
/// on `typedpg_analyzer`.
pub(crate) mod qualified_name {
    pub use typedpg_core::QualifiedName;
}

pub use ddl::acl::Privilege;
pub use oid::{
    PgCastOid, PgClassOid, PgCollationOid, PgConstraintOid, PgEnumOid, PgExtensionOid,
    PgGenericOid, PgLanguageOid, PgNamespaceOid, PgOpclassOid, PgOperatorOid, PgProcOid,
    PgRewriteOid, PgTypeOid,
};
#[cfg(any(test, feature = "internal"))]
pub use pg_catalog::{
    AclCatalog, AggKind, ArgMode, AstBinding, AttGenerated, AttIdentity, CastContext, CastMethod,
    ConType, DepType, EvEnabled, EvType, PgAggregate, PgAm, PgAmop, PgAttribute, PgCast,
    PgCatalogSeed, PgClass, PgCollation, PgConstraint, PgConversion, PgDepend, PgEnum, PgExtension,
    PgIndex, PgInherits, PgLanguage, PgNamespace, PgOpclass, PgOperator, PgOpfamily, PgProc,
    PgRange, PgRewrite, PgSetting, PgTsConfigMap, PgTsObject, PgType, ProKind, ProVolatile,
    RelKind, SeedAcl, SerializedAst, TypAlign, TypCategory, TypStorage, TypType,
};

pub use analysis_diff::{AnalysisDifference, first_difference};
pub use condition::{
    AssignmentViolation, CheckPart, CheckReading, ConditionFact, ConditionReason, PartOutcome,
    WriteBase, WriteCommand, WriteConstraint, WriteStatement, WrittenColumn, WrittenValue,
};
pub use ddl::DdlError;
pub use error::AnalyzeError;
pub use pg_catalog::{
    CoalesceFact, ColumnRefBinding, LocatedError, MigrationObserver, MigrationStatement,
    NullifiedJoin, PgCatalog, QueryView, RelationRef, SublinkNullability, UserObjectCounts,
    ValueLimit,
};
pub use refine::{IntRange, Refinement, Widenings};
pub use refine_bounds::{Num, OrdBound, OrdKind, OrdPoint, OrdRange};
pub use resolve::{
    AnalyzedColumn, AnalyzedCopyIn, AnalyzedParam, AnalyzedQuery, AnalyzedSpread,
    AnalyzedSpreadField, ArrayForm, SpreadKind, ValuesItem, ValuesList,
};
pub use simplify::{
    AppliedSimplification, OrderKey, OrderTies, RedundantJoin, RedundantJoinProof, Rule,
    SimplifyOptions, order_keys, order_ties, redundant_distinct_spans, redundant_group_keys,
    relation_holds_at_most_one_row, select_at_most_one_row, statement_at_most_one_row,
    statement_node_at_most_one_row, unique_order_prefix,
};
pub use sql_text::strip_trailing_junk;
/// The "did you mean" matcher behind the analyzer's suggestions, for the
/// macros' own errors (an unknown `sql!` argument name).
pub use suggest::suggest_similar;
pub use template::{TemplateParam, template_params};
pub use type_facts::{OperatorTypes, ValueConversion};
pub use typedpg_core::{ParseQualifiedNameError, QualifiedName};
pub use types::{RecordField, Type};
