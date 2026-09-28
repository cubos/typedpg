//! Shared setup, helpers, and assertion utilities for analyzer tests.

#![allow(dead_code, unused_imports)]

pub use typedpg_analyzer::{
    AnalyzeError, AnalyzedColumn, AnalyzedQuery, AttGenerated, CastContext, CastMethod, DdlError,
    PgAggregate, PgAttribute, PgCast, PgCastOid, PgCatalog, PgCatalogSeed, PgClass, PgClassOid,
    PgDepend, PgEnum, PgEnumOid, PgExtension, PgExtensionOid, PgGenericOid, PgIndex, PgNamespace,
    PgNamespaceOid, PgOperator, PgOperatorOid, PgProc, PgProcOid, PgRange, PgType, PgTypeOid,
    ProKind, QualifiedName, RecordField, RelKind, TypCategory, TypType, Type,
};

/// Terse helper for building a [`QualifiedName`] in tests.
pub fn qn(schema: &str, name: &str) -> QualifiedName {
    QualifiedName::new(schema, name)
}

/// Look up a relation by name and return its OID. Panics if not found.
#[track_caller]
pub fn class_oid(db: &PgCatalog, schema: Option<&str>, name: &str) -> PgClassOid {
    db.resolve_table(schema, name)
        .unwrap_or_else(|| panic!("relation {schema:?}.{name} not found"))
        .oid
}

/// Tables that a view depends on, as `(schema, name)` pairs.
pub fn view_table_deps(db: &PgCatalog, view_oid: PgClassOid) -> Vec<QualifiedName> {
    let out: std::collections::BTreeSet<QualifiedName> = db
        .view_dependencies(view_oid)
        .filter_map(|(refobj, _)| {
            let class = db.pg_class().get(&refobj)?;
            let schema = db.namespace_name(class.relnamespace)?;
            Some(QualifiedName::new(schema, class.relname.clone()))
        })
        .collect();
    out.into_iter().collect()
}

/// Column-level deps a view has, as `(QualifiedName, column_name)` pairs.
pub fn view_column_deps(db: &PgCatalog, view_oid: PgClassOid) -> Vec<(QualifiedName, String)> {
    let out: std::collections::BTreeSet<(QualifiedName, String)> = db
        .view_dependencies(view_oid)
        .filter_map(|(refobj, refobjsubid)| {
            if refobjsubid == 0 {
                return None;
            }
            let class = db.pg_class().get(&refobj)?;
            let schema = db.namespace_name(class.relnamespace)?;
            let attr = db
                .attributes_of(refobj)
                .iter()
                .find(|a| a.attnum == refobjsubid)?;
            Some((
                QualifiedName::new(schema, class.relname.clone()),
                attr.attname.clone(),
            ))
        })
        .collect();
    out.into_iter().collect()
}

// ──────────────────────────────────────────────────────────────────────────────
// Multi-migration setup (DDL tests)
// ──────────────────────────────────────────────────────────────────────────────

/// Apply a sequence of `(filename, sql)` migrations and return the resulting
/// catalog. Panics on any migration error — use [`try_apply`] when the test
/// expects a specific failure.
pub fn build(migrations: &[(&str, &str)]) -> PgCatalog {
    build_db(migrations)
}

/// Apply a sequence of migrations and return the live [`PgCatalog`] — useful
/// when the test also needs to analyze queries against the resulting schema.
pub fn build_db(migrations: &[(&str, &str)]) -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    for (_, sql) in migrations {
        db.apply_sql(sql).unwrap();
    }
    db
}

/// Apply a sequence of migrations and return the `Result` so tests can match
/// on the specific [`DdlError`] variant.
pub fn try_apply(migrations: &[(&str, &str)]) -> Result<(), DdlError> {
    let mut db = PgCatalog::new().unwrap();
    for (_, sql) in migrations {
        db.apply_sql(sql)?;
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// Type constructors — reduce boilerplate in asserts
// ──────────────────────────────────────────────────────────────────────────────

pub fn basic(schema: &str, name: &str) -> Type {
    Type::Basic {
        schema: schema.into(),
        name: name.into(),
        extension: None,
        typmod: None,
        collation: None,
    }
}

pub fn basic_with_typmod(schema: &str, name: &str, typmod: i32) -> Type {
    Type::Basic {
        schema: schema.into(),
        name: name.into(),
        extension: None,
        typmod: Some(typmod),
        collation: None,
    }
}

pub fn basic_ext(schema: &str, name: &str, extension: &str) -> Type {
    Type::Basic {
        schema: schema.into(),
        name: name.into(),
        extension: Some(extension.into()),
        typmod: None,
        collation: None,
    }
}

pub fn basic_with_collation(schema: &str, name: &str, collation: &str) -> Type {
    Type::Basic {
        schema: schema.into(),
        name: name.into(),
        extension: None,
        typmod: None,
        collation: Some(collation.into()),
    }
}

pub fn array_of(element: Type) -> Type {
    Type::Array {
        element: Box::new(element),
    }
}

pub fn domain(schema: &str, name: &str, base: Type) -> Type {
    Type::Domain {
        schema: schema.into(),
        name: name.into(),
        base: Box::new(base),
        extension: None,
        typmod: None,
        collation: None,
    }
}

pub fn domain_with_collation(schema: &str, name: &str, base: Type, collation: &str) -> Type {
    Type::Domain {
        schema: schema.into(),
        name: name.into(),
        base: Box::new(base),
        extension: None,
        typmod: None,
        collation: Some(collation.into()),
    }
}

pub fn enum_ty(schema: &str, name: &str, labels: &[&str]) -> Type {
    Type::Enum {
        schema: schema.into(),
        name: name.into(),
        labels: labels.iter().map(|s| s.to_string()).collect(),
        extension: None,
    }
}

pub fn range_of(schema: &str, name: &str, subtype: Type) -> Type {
    Type::Range {
        schema: schema.into(),
        name: name.into(),
        subtype: Box::new(subtype),
        extension: None,
        typmod: None,
    }
}

/// Build a [`Type::AnonymousRecord`] from a list of [`RecordField`]s.
/// Pair with [`rf`] / [`rfn`] for terse construction.
pub fn anon_record(fields: Vec<RecordField>) -> Type {
    Type::AnonymousRecord { fields }
}

/// Build a named [`Type::Composite`] (registered via `CREATE TYPE … AS (…)`
/// or as the implicit row type of a table). Pair with [`rf`] / [`rfn`].
pub fn composite(schema: &str, name: &str, fields: Vec<RecordField>) -> Type {
    Type::Composite {
        schema: schema.into(),
        name: name.into(),
        fields,
        extension: None,
    }
}

/// Non-null record field (mirrors [`c`] for record fields).
pub fn rf(name: &str, ty: Type) -> RecordField {
    RecordField {
        name: name.into(),
        ty,
        nullable: false,
    }
}

/// Nullable record field (mirrors [`cn`] for record fields).
pub fn rfn(name: &str, ty: Type) -> RecordField {
    RecordField {
        name: name.into(),
        ty,
        nullable: true,
    }
}

// Commonly used built-ins (short aliases).

pub fn bool_ty() -> Type {
    basic("pg_catalog", "bool")
}
pub fn bytea() -> Type {
    basic("pg_catalog", "bytea")
}
pub fn int2() -> Type {
    basic("pg_catalog", "int2")
}
pub fn int4() -> Type {
    basic("pg_catalog", "int4")
}
pub fn int8() -> Type {
    basic("pg_catalog", "int8")
}
pub fn float4() -> Type {
    basic("pg_catalog", "float4")
}
pub fn float8() -> Type {
    basic("pg_catalog", "float8")
}
pub fn numeric() -> Type {
    basic("pg_catalog", "numeric")
}
/// Build a `numeric(p, s)` type with the matching typmod.
pub fn numeric_ps(precision: i32, scale: i32) -> Type {
    basic_with_typmod(
        "pg_catalog",
        "numeric",
        ((precision << 16) | (scale & 0xFFFF)) + 4,
    )
}
pub fn text() -> Type {
    basic("pg_catalog", "text")
}
pub fn varchar() -> Type {
    basic("pg_catalog", "varchar")
}
/// Build a `varchar(n)` type with the matching typmod.
pub fn varchar_n(length: i32) -> Type {
    basic_with_typmod("pg_catalog", "varchar", length + 4)
}
pub fn bpchar() -> Type {
    basic("pg_catalog", "bpchar")
}
pub fn name_ty() -> Type {
    basic("pg_catalog", "name")
}
pub fn uuid() -> Type {
    basic("pg_catalog", "uuid")
}
pub fn date() -> Type {
    basic("pg_catalog", "date")
}
pub fn time_ty() -> Type {
    basic("pg_catalog", "time")
}
pub fn timestamp() -> Type {
    basic("pg_catalog", "timestamp")
}
pub fn timestamptz() -> Type {
    basic("pg_catalog", "timestamptz")
}
pub fn interval() -> Type {
    basic("pg_catalog", "interval")
}
pub fn json_ty() -> Type {
    basic("pg_catalog", "json")
}
pub fn jsonb() -> Type {
    basic("pg_catalog", "jsonb")
}
pub fn oid_ty() -> Type {
    basic("pg_catalog", "oid")
}
pub fn unknown() -> Type {
    basic("pg_catalog", "unknown")
}
pub fn void() -> Type {
    basic("pg_catalog", "void")
}

// ──────────────────────────────────────────────────────────────────────────────
// Column / param specs for concise shape asserts
// ──────────────────────────────────────────────────────────────────────────────

/// What a test expects a column to look like. The PG cast name is
/// mechanically derived from [`Type`] via [`Type::cast_name`], so it isn't
/// spelled out here — consumers that care about the cast can call it
/// directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColSpec {
    pub name: String,
    pub ty: Type,
    pub nullable: bool,
}

/// Non-null column.
pub fn c(name: &str, ty: Type) -> ColSpec {
    ColSpec {
        name: name.into(),
        ty,
        nullable: false,
    }
}

/// Nullable column.
pub fn cn(name: &str, ty: Type) -> ColSpec {
    ColSpec {
        name: name.into(),
        ty,
        nullable: true,
    }
}

/// Assert the query's output columns match the expected specs exactly.
#[track_caller]
pub fn assert_cols(analyzed: &AnalyzedQuery, expected: Vec<ColSpec>) {
    let actual: Vec<ColSpec> = analyzed
        .columns
        .iter()
        .map(|c| ColSpec {
            name: c.name.clone(),
            ty: c.pg_type.clone(),
            nullable: c.nullable,
        })
        .collect();
    assert_eq!(actual, expected, "columns mismatch");
}

/// What a test expects a param to look like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamSpec {
    pub ty: Type,
    pub nullable: bool,
}

/// Non-null param, no cast.
pub fn p(ty: Type) -> ParamSpec {
    ParamSpec {
        ty,
        nullable: false,
    }
}

/// Nullable param, no cast.
pub fn pn(ty: Type) -> ParamSpec {
    ParamSpec { ty, nullable: true }
}

/// Assert the query's params match the expected specs (order-sensitive).
#[track_caller]
pub fn assert_params(analyzed: &AnalyzedQuery, expected: Vec<ParamSpec>) {
    let actual: Vec<ParamSpec> = analyzed
        .params
        .iter()
        .map(|p| ParamSpec {
            ty: p.pg_type.clone(),
            nullable: p.nullable,
        })
        .collect();
    assert_eq!(actual, expected, "params mismatch");
}

// ──────────────────────────────────────────────────────────────────────────────
// Lookup helpers
// ──────────────────────────────────────────────────────────────────────────────

/// Assert an individual column by name has the expected type + nullability
/// without having to spell out every sibling column — useful for stress
/// tests that project dozens of columns but only care about a few.
///
/// ```ignore
/// assert_col!(info, "next_id", int8(), nullable = false);
/// assert_col!(info, "safe_age", int4(), nullable = true);
/// ```
#[macro_export]
macro_rules! assert_col {
    ($info:expr, $name:expr, $ty:expr, nullable = $nullable:expr $(,)?) => {{
        let col = $crate::common::col($info, $name);
        assert_eq!(col.pg_type, $ty, "column '{}' type mismatch", $name);
        assert_eq!(
            col.nullable, $nullable,
            "column '{}' nullability mismatch",
            $name
        );
    }};
}

pub fn col<'a>(info: &'a AnalyzedQuery, name: &str) -> &'a AnalyzedColumn {
    info.columns
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| {
            panic!(
                "column '{name}' not found in: {:?}",
                info.columns.iter().map(|c| &c.name).collect::<Vec<_>>()
            )
        })
}

/// Assert two [`AnalyzedQuery`]s have identical types (ignoring nullability).
#[track_caller]
pub fn assert_same_types(a: &AnalyzedQuery, b: &AnalyzedQuery, context: &str) {
    assert_eq!(
        a.columns.len(),
        b.columns.len(),
        "{context}: column count mismatch"
    );
    for (ca, cb) in a.columns.iter().zip(b.columns.iter()) {
        assert_eq!(ca.name, cb.name, "{context}: column name mismatch");
        assert_eq!(
            ca.pg_type, cb.pg_type,
            "{context}: type mismatch for column '{}'",
            ca.name
        );
    }
    assert_eq!(
        a.params.len(),
        b.params.len(),
        "{context}: param count mismatch"
    );
    for (i, (pa, pb)) in a.params.iter().zip(b.params.iter()).enumerate() {
        assert_eq!(pa.pg_type, pb.pg_type, "{context}: param {i} type mismatch");
    }
}

/// Assert two [`AnalyzedQuery`]s are completely identical (types + nullability).
#[track_caller]
pub fn assert_identical(a: &AnalyzedQuery, b: &AnalyzedQuery, context: &str) {
    assert_same_types(a, b, context);
    for (ca, cb) in a.columns.iter().zip(b.columns.iter()) {
        assert_eq!(
            ca.nullable, cb.nullable,
            "{context}: nullability mismatch for column '{}' (a={}, b={})",
            ca.name, ca.nullable, cb.nullable
        );
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Error assertions
//
// Every test that expects an error must validate BOTH the variant (via a
// `matches!` pattern) AND a stable substring of the message. This catches
// regressions where an error changes from e.g. `DuplicateObject` to `Parse`,
// which the old `assert!(result.is_err())` pattern would miss.
// ──────────────────────────────────────────────────────────────────────────────

/// Assert a query fails with a `TypeMismatch` whose `actual`/`expected` fields
/// match exactly. Used for the analyzer's structured type-coercion errors.
#[track_caller]
pub fn assert_type_mismatch(db: &PgCatalog, sql: &str, expect_actual: &str, expect_expected: &str) {
    match db.analyze(sql) {
        Err(AnalyzeError::TypeMismatch {
            actual,
            expected,
            context,
        }) => {
            assert_eq!(
                actual, expect_actual,
                "TypeMismatch.actual wrong for: {sql}\n  context: {context}"
            );
            assert_eq!(
                expected, expect_expected,
                "TypeMismatch.expected wrong for: {sql}\n  context: {context}"
            );
            assert!(
                !context.is_empty(),
                "TypeMismatch.context is empty for: {sql}"
            );
        }
        Err(other) => panic!(
            "expected TypeMismatch({expect_actual} -> {expect_expected}) for: {sql}\n  got: {other}"
        ),
        Ok(info) => panic!(
            "expected TypeMismatch({expect_actual} -> {expect_expected}) for: {sql}\n  \
             got params: {:?}\n  got columns: {:?}",
            info.params
                .iter()
                .map(|p| p.pg_type.clone())
                .collect::<Vec<_>>(),
            info.columns
                .iter()
                .map(|c| (c.name.clone(), c.pg_type.clone()))
                .collect::<Vec<_>>(),
        ),
    }
}

/// Assert a query fails with a specific `AnalyzeError` variant and the exact
/// expected message.
///
/// Use via the [`assert_analyze_err!`] macro for natural pattern syntax:
/// ```ignore
/// assert_analyze_err!(db.analyze(sql), AnalyzeError::UndefinedTable(_), "relation \"users\" does not exist");
/// ```
#[macro_export]
macro_rules! assert_analyze_err {
    ($result:expr, $variant:pat, $expected_msg:expr $(,)?) => {{
        // Borrow the expression so the macro doesn't consume the caller's
        // binding — callers often inspect the error again afterwards.
        match &$result {
            Err(err) => {
                if !matches!(err, $variant) {
                    panic!(
                        "expected {} but got different variant: {:?}",
                        stringify!($variant),
                        err
                    );
                }
                let msg = err.to_string();
                let expected: &str = $expected_msg;
                assert_eq!(
                    msg, expected,
                    "error message mismatch\n  expected: {:?}\n       got: {:?}",
                    expected, msg,
                );
            }
            Ok(info) => panic!(
                "expected {} with message {:?}, got Ok: {:?}",
                stringify!($variant),
                $expected_msg,
                info,
            ),
        }
    }};
}

/// Assert a migration `Result<(), DdlError>` failed with a specific variant
/// and the **exact, complete** error message (like [`assert_analyze_err!`]) —
/// so the test pins the full wording and we get visibility into message
/// quality, not just a substring.
///
/// ```ignore
/// assert_ddl_err!(try_apply(&[...]), DdlError::DuplicateObject(_),
///                 "column \"name\" of relation \"t\" already exists");
/// ```
#[macro_export]
macro_rules! assert_ddl_err {
    ($result:expr, $variant:pat, $expected_msg:expr $(,)?) => {{
        // Borrow the expression so the macro doesn't consume the caller's
        // binding — callers often inspect the error again afterwards.
        match &$result {
            Err(err) => {
                if !matches!(err, $variant) {
                    panic!(
                        "expected {} but got different variant: {:?}",
                        stringify!($variant),
                        err
                    );
                }
                let msg = err.to_string();
                let expected: &str = $expected_msg;
                assert_eq!(
                    msg, expected,
                    "DDL error message mismatch\n  expected: {:?}\n       got: {:?}",
                    expected, msg,
                );
            }
            Ok(()) => panic!(
                "expected {} with message {:?}, got Ok(())",
                stringify!($variant),
                $expected_msg,
            ),
        }
    }};
}

// `#[macro_export]` macros land at the crate root of the test binary, so any
// test file in this binary can invoke them as `assert_ddl_err!(...)` — no
// `use` needed, and `mod common;` in the binary's entry file is enough to
// make them visible.

/// Like [`assert_analyze_err!`] but only requires the rendered message to
/// *start with* the expected PG-verbatim wording (diagnostics append spans
/// and hints after it).
#[macro_export]
macro_rules! assert_err_prefix {
    ($result:expr, $variant:pat, $expected_prefix:expr $(,)?) => {{
        match &$result {
            Err(err) => {
                assert!(
                    matches!(err, $variant),
                    "expected {} but got: {:?}",
                    stringify!($variant),
                    err
                );
                let msg = err.to_string();
                let expected: &str = $expected_prefix;
                assert!(
                    msg.starts_with(expected),
                    "error message mismatch\n  expected prefix: {:?}\n              got: {:?}",
                    expected,
                    msg,
                );
            }
            Ok(info) => panic!(
                "expected {} with message {:?}, got Ok: {:?}",
                stringify!($variant),
                $expected_prefix,
                info,
            ),
        }
    }};
}
