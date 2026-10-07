//! One query's entries in the generated module: its member of the
//! `Queries` (or `CopyTargets`) interface — the types — and of the runtime
//! table — the SQL and codecs.

use std::collections::HashSet;
use std::fmt::Write as _;

use serde_json::{Value, json};
use typedpg_analyzer::{
    AnalyzedColumn, AnalyzedCopyIn, AnalyzedQuery, LocatedError, PgCatalog, Type,
};

use crate::scan::Kind;
use crate::typemap::{TypeMapper, is_anonymous_record, property_key};

/// A query's generated entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generated {
    /// The member's type: `{ params: …; row: …; value: … }` for a query,
    /// `{ row: … }` for a COPY target.
    pub types: String,
    /// The runtime table's entry, as JSON: `{"sql":[…],"params":[…],…}`. A
    /// string in the generated module rather than an object literal: tsc
    /// then has one string type to check per query instead of a tree of
    /// tuples, which halved its check time on 20k queries.
    pub runtime: String,
}

/// Why a query has no entries: the message to report, and where in the
/// query it points, when it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenError {
    pub message: String,
    /// A byte offset in the query's text.
    pub offset: Option<usize>,
}

impl GenError {
    fn plain(message: impl Into<String>) -> Self {
        GenError {
            message: message.into(),
            offset: None,
        }
    }
}

impl From<LocatedError> for GenError {
    fn from(e: LocatedError) -> Self {
        GenError {
            message: e.error.to_string(),
            offset: e.offset,
        }
    }
}

/// Analyze `text` and generate its entries.
pub fn generate(
    catalog: &PgCatalog,
    mapper: &TypeMapper,
    kind: Kind,
    text: &str,
) -> Result<Generated, GenError> {
    match kind {
        Kind::Query => generate_query(&catalog.analyze_located(text)?, mapper),
        Kind::CopyIn => generate_copy_in(&catalog.analyze_copy_in_located(text)?, mapper),
    }
}

const RECORD_PARAM: &str = "anonymous record-typed query parameters are not supported — pass \
                            the fields individually using a ROW(...) constructor in SQL";

fn generate_query(a: &AnalyzedQuery, mapper: &TypeMapper) -> Result<Generated, GenError> {
    // A native `$1` placeholder is a valid PG parameter, but it has no name
    // a parameter could bind to.
    if let Some(p) = a
        .params
        .iter()
        .find(|p| p.name.starts_with(|c: char| c.is_ascii_digit()))
    {
        return Err(GenError::plain(format!(
            "positional placeholder `${}` is not supported in sql(): name the parameter (e.g. \
             `$id`) so it can be bound",
            p.name
        )));
    }
    for spread in &a.spreads {
        let mut seen = HashSet::new();
        for field in &spread.fields {
            if !seen.insert(field.name.as_str()) {
                return Err(GenError::plain(format!(
                    "duplicate field '{}' in $..{} spread",
                    field.name, spread.name
                )));
            }
        }
    }
    check_unique_columns(a.columns.iter().map(|c| c.name.as_str()))?;
    // PG has no input for an anonymous record: a `record` parameter can't
    // be bound (a named composite can, from its text form).
    let anonymous = a
        .params
        .iter()
        .map(|p| &p.pg_type)
        .chain(
            a.spreads
                .iter()
                .flat_map(|s| s.fields.iter().map(|f| &f.pg_type)),
        )
        .any(is_anonymous_record);
    if anonymous {
        return Err(GenError::plain(RECORD_PARAM));
    }

    // ── types ──
    let mut params_ty = Vec::new();
    for p in &a.params {
        let m = mapper.map(&p.pg_type);
        params_ty.push(member(&p.name, &m.input, p.nullable));
    }
    for s in &a.spreads {
        let fields: Vec<String> = s
            .fields
            .iter()
            .map(|f| member(&f.name, &mapper.map(&f.pg_type).input, f.nullable))
            .collect();
        params_ty.push(format!(
            "{}: readonly {{ {} }}[]",
            property_key(&s.name),
            fields.join("; ")
        ));
    }
    let (row_ty, columns_rt) = columns(&a.columns, mapper);
    let mut types = format!(
        "{{\n    params: {};\n    row: {};\n",
        object(&params_ty, 4),
        object(&row_ty, 4)
    );
    if let [c] = a.columns.as_slice() {
        let m = mapper.map(&c.pg_type);
        let _ = writeln!(types, "    value: {};", nullable(&m.output, c.nullable));
    }
    types.push_str("  }");

    // ── runtime ──
    let mut pieces = Vec::new();
    let mut last = 0;
    for s in &a.spreads {
        pieces.push(cast_range(a, last, s.offset));
        last = s.offset;
    }
    pieces.push(cast_range(a, last, a.sql.len()));
    let mut spec = serde_json::Map::new();
    spec.insert("sql".into(), json!(pieces));
    spec.insert(
        "params".into(),
        a.params
            .iter()
            .map(|p| json!([p.name, mapper.map(&p.pg_type).codec.to_json()]))
            .collect(),
    );
    if !a.spreads.is_empty() {
        spec.insert(
            "spreads".into(),
            a.spreads
                .iter()
                .map(|s| {
                    let fields: Vec<Value> = s
                        .fields
                        .iter()
                        .map(|f| {
                            json!([
                                f.name,
                                mapper.map(&f.pg_type).codec.to_json(),
                                cast_suffix(&f.pg_type)
                            ])
                        })
                        .collect();
                    json!([s.name, fields])
                })
                .collect(),
        );
    }
    spec.insert("columns".into(), Value::Array(columns_rt));
    // `fetchOne` / `fetchOptional` wrap the query in `LIMIT 2` when PG
    // accepts it as a subquery, as the Rust side does; never with spreads.
    if a.can_run_as_subquery && a.spreads.is_empty() {
        spec.insert("subquery".into(), json!(true));
    }
    Ok(Generated {
        types,
        runtime: Value::Object(spec).to_string(),
    })
}

fn generate_copy_in(c: &AnalyzedCopyIn, mapper: &TypeMapper) -> Result<Generated, GenError> {
    if c.columns
        .iter()
        .any(|col| is_anonymous_record(&col.pg_type))
    {
        return Err(GenError::plain(RECORD_PARAM));
    }
    // The text format: the analyzer writes the binary one, which the Rust
    // side streams.
    let copy_sql = c.copy_sql.strip_suffix(" (FORMAT binary)").ok_or_else(|| {
        GenError::plain(format!(
            "internal error: unexpected COPY statement `{}`",
            c.copy_sql
        ))
    })?;
    let row: Vec<String> = c
        .columns
        .iter()
        .map(|col| member(&col.name, &mapper.map(&col.pg_type).input, col.nullable))
        .collect();
    let columns: Vec<Value> = c
        .columns
        .iter()
        .map(|col| json!([col.name, mapper.map(&col.pg_type).codec.to_json()]))
        .collect();
    Ok(Generated {
        types: format!("{{\n    row: {};\n  }}", object(&row, 4)),
        runtime: json!({ "sql": copy_sql, "columns": columns }).to_string(),
    })
}

/// A row object has one property per name.
fn check_unique_columns<'a>(names: impl Iterator<Item = &'a str>) -> Result<(), GenError> {
    let mut seen = HashSet::new();
    for name in names {
        if !seen.insert(name) {
            return Err(GenError::plain(format!(
                "duplicate column name \"{name}\" in the query's output: a row object has one \
                 property per name, so give the columns distinct names (`AS ...`)"
            )));
        }
    }
    Ok(())
}

/// The row type's members and the runtime's column codecs.
fn columns(cols: &[AnalyzedColumn], mapper: &TypeMapper) -> (Vec<String>, Vec<Value>) {
    let mut row = Vec::new();
    let mut codecs = Vec::new();
    for c in cols {
        let m = mapper.map(&c.pg_type);
        row.push(member(&c.name, &m.output, c.nullable));
        codecs.push(json!([c.name, m.codec.to_json()]));
    }
    (row, codecs)
}

fn nullable(ty: &str, nullable: bool) -> String {
    if nullable {
        format!("{ty} | null")
    } else {
        ty.to_owned()
    }
}

fn member(name: &str, ty: &str, is_nullable: bool) -> String {
    format!("{}: {}", property_key(name), nullable(ty, is_nullable))
}

/// `{ a: T; b: U }` over several lines, or `{}`.
fn object(members: &[String], indent: usize) -> String {
    if members.is_empty() {
        return "{}".to_owned();
    }
    let pad = " ".repeat(indent + 2);
    let mut out = String::from("{\n");
    for m in members {
        let _ = writeln!(out, "{pad}{m};");
    }
    out.push_str(&" ".repeat(indent));
    out.push('}');
    out
}

/// `::type` for a spread field's placeholder, as the Rust side writes it.
fn cast_suffix(ty: &Type) -> String {
    ty.cast_name().map(|n| format!("::{n}")).unwrap_or_default()
}

/// `a.sql[start..end]` with each regular parameter placeholder cast to its
/// type, `($1::pg_catalog.int4)` — what the Rust side sends (see
/// `typedpg_macros::codegen::cast_range`): the text a parameter is bound as
/// is then read as its type, whatever the driver declares it as.
fn cast_range(a: &AnalyzedQuery, start: usize, end: usize) -> String {
    let sql = &a.sql;
    let mut insertions: Vec<(usize, String)> = Vec::new();
    for param in &a.params {
        if let Some(pg_type) = param.pg_type.cast_name() {
            for &offset in &param.sql_offsets {
                if offset > start && offset <= end {
                    let digits = sql.as_bytes()[..offset]
                        .iter()
                        .rev()
                        .take_while(|b| b.is_ascii_digit())
                        .count();
                    let placeholder = offset - digits - 1;
                    insertions.push((placeholder, "(".to_owned()));
                    insertions.push((offset, format!("::{pg_type})")));
                }
            }
        }
    }
    insertions.sort_by_key(|(off, _)| *off);
    let mut out = String::with_capacity(end - start + insertions.len() * 8);
    let mut last = start;
    for (offset, s) in &insertions {
        out.push_str(&sql[last..*offset]);
        out.push_str(s);
        last = *offset;
    }
    out.push_str(&sql[last..end]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Int8;
    use std::collections::HashMap;

    const DDL: &str = "CREATE TYPE mood AS ENUM ('happy', 'sad');
        CREATE TYPE address AS (street text, num int4);
        CREATE DOMAIN tags_d AS text[];
        CREATE TABLE users (id int4 PRIMARY KEY, name text, big int8 NOT NULL, m mood,
                            tags text[] NOT NULL, prefs jsonb, home address,
                            moods mood[], span int4range, period tstzmultirange,
                            t2 tags_d NOT NULL DEFAULT '{}', homes address[]);";

    fn run(kind: Kind, sql: &str) -> Result<Generated, GenError> {
        let mut c = PgCatalog::new().unwrap();
        c.apply_migration("0001.sql", DDL).unwrap();
        let overrides = HashMap::new();
        let mapper = TypeMapper {
            overrides: &overrides,
            int8: Int8::Bigint,
        };
        generate(&c, &mapper, kind, sql)
    }

    fn ok(sql: &str) -> Generated {
        run(Kind::Query, sql).unwrap()
    }

    fn err(sql: &str) -> GenError {
        run(Kind::Query, sql).unwrap_err()
    }

    #[test]
    fn select_with_param() {
        let g = ok("SELECT id, name, big, m, tags, prefs FROM users WHERE id = $id");
        assert_eq!(
            g.types,
            r#"{
    params: {
      id: number;
    };
    row: {
      id: number;
      name: string | null;
      big: bigint;
      m: "happy" | "sad" | null;
      tags: (string | null)[];
      prefs: typedpg.JsonValue | null;
    };
  }"#
        );
        assert_eq!(
            g.runtime,
            r#"{"sql":["SELECT id, name, big, m, tags, prefs FROM users WHERE id = ($1::pg_catalog.int4)"],"params":[["id","number"]],"columns":[["id","number"],["name","text"],["big","bigint"],["m","text"],["tags",["array","text"]],["prefs","json"]],"subquery":true}"#
        );
    }

    #[test]
    fn hard_types() {
        let g = ok("SELECT home, moods, span, period, t2, homes FROM users");
        for expected in [
            "home: { street: string | null; num: number | null } | null;",
            r#"moods: ("happy" | "sad" | null)[] | null;"#,
            "span: typedpg.Range<number> | null;",
            "period: typedpg.Range<Date>[] | null;",
            "t2: (string | null)[];",
            "homes: ({ street: string | null; num: number | null } | null)[] | null;",
        ] {
            assert!(g.types.contains(expected), "{expected}\n{}", g.types);
        }
        assert!(
            g.runtime.contains(r#"["span",["range","number"]]"#),
            "{}",
            g.runtime
        );
        assert!(
            g.runtime
                .contains(r#"["period",["multirange","timestamptz"]]"#),
            "{}",
            g.runtime
        );
    }

    #[test]
    fn composite_parameters_are_bound_from_text() {
        let g = ok("UPDATE users SET home = $home, homes = $homes WHERE id = $id");
        assert!(
            g.types
                .contains("home: { street: string | null; num: number | null } | null;"),
            "{}",
            g.types
        );
        assert!(g.runtime.contains("($1::public.address)"), "{}", g.runtime);
    }

    #[test]
    fn nullability_annotations() {
        let g = ok(r#"SELECT name AS "name!", id AS "id?" FROM users WHERE name = $n?"#);
        assert!(g.types.contains("name: string;"), "{}", g.types);
        assert!(g.types.contains("id: number | null;"), "{}", g.types);
        assert!(g.types.contains("n: string | null;"), "{}", g.types);
    }

    #[test]
    fn single_column_has_a_value() {
        let g = ok("SELECT count(*) AS n FROM users");
        assert!(g.types.contains("value: bigint;"), "{}", g.types);
    }

    #[test]
    fn spreads() {
        let g = ok(
            "INSERT INTO users (id, name, big, tags) VALUES $..rows { id, name, big, tags } \
                    RETURNING id",
        );
        assert!(
            g.types.contains("rows: readonly { id: number; name: string | null; big: bigint | number | string; tags: readonly (string | null)[] }[];"),
            "{}",
            g.types
        );
        assert!(g.runtime.contains(
            r#""spreads":[["rows",[["id","number","::pg_catalog.int4"],["name","text","::pg_catalog.text"],["big","bigint","::pg_catalog.int8"],["tags",["array","text"],"::pg_catalog.text[]"]]]]"#
        ), "{}", g.runtime);
        assert!(!g.runtime.contains("subquery"));
    }

    #[test]
    fn copy_in() {
        let g = run(Kind::CopyIn, "users (id, name, home)").unwrap();
        assert_eq!(
            g.types,
            "{\n    row: {\n      id: number;\n      name: string | null;\n      home: { street: string | null; num: number | null } | null;\n    };\n  }"
        );
        assert_eq!(
            g.runtime,
            r#"{"sql":"COPY public.users (id, name, home) FROM STDIN","columns":[["id","number"],["name","text"],["home",["record",[["street","text"],["num","number"]]]]]}"#
        );
        let e = run(Kind::CopyIn, "users (id, nope)").unwrap_err();
        assert!(e.message.contains("\"nope\""), "{}", e.message);
    }

    #[test]
    fn errors() {
        let e = err("SELECT nmae FROM users");
        assert!(e.message.starts_with("column \"nmae\" does not exist"));
        assert_eq!(e.offset, Some(7));
        assert!(
            err("SELECT 1 FROM users WHERE id = $1")
                .message
                .contains("positional placeholder")
        );
        assert!(
            err("SELECT id, id FROM users")
                .message
                .contains("duplicate column name \"id\"")
        );
        assert!(
            err("SELECT $r::record")
                .message
                .contains("anonymous record")
        );
    }
}
