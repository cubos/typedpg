//! One query's entries in the generated module: its member of the
//! `Queries` (or `CopyTargets`) interface — the types — and of the runtime
//! table — the SQL and codecs.

use std::collections::HashSet;
use std::fmt::Write as _;

use serde_json::{Value, json};
use typedpg_analyzer::{
    AnalyzedColumn, AnalyzedCopyIn, AnalyzedQuery, LocatedError, PgCatalog, SpreadKind, Type,
    ValuesItem, ValuesList, first_difference,
};

use crate::scan::Kind;
use crate::typemap::{TypeMapper, is_anonymous_record, property_key};

/// A query's generated entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generated {
    /// The member's type: `{ params: …; row: …; value: … }` for a query,
    /// `{ row: … }` for a COPY target.
    pub types: String,
    /// The members of `params` (none for a COPY target), `name: type`.
    pub params: Vec<String>,
    /// The members of `row`.
    pub row: Vec<String>,
    /// The single column's type, for a query with one column.
    pub value: Option<String>,
    /// The TS names of the `types` overrides the types use, sorted.
    pub overrides: Vec<String>,
    /// The runtime table's entry, as JSON: `{"sql":[…],"params":[…],…}`. A
    /// string in the generated module rather than an object literal: tsc
    /// then has one string type to check per query instead of a tree of
    /// tuples, which halved its check time on 20k queries.
    pub runtime: String,
    /// When the SQL sent is a simplification of the one written: a note
    /// naming the rules applied, with the SQL sent, for the generated
    /// module's doc comments.
    pub simplified: Option<String>,
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

/// Analyze `text` and generate its entries. `roles` are the database's
/// configured roles: the query is analyzed as the first of them (or as the
/// schema's owner when there are none) and generated from that analysis, so
/// the role's search path decides what its unqualified names mean. Every
/// other role's analysis must agree on the types — a query one may run but
/// another resolves to different types cannot be generated — and a role that
/// may not run the query is the error (`PgCatalog::analyze_as`). A COPY
/// target's privileges are not modeled, so its roles are not checked.
pub fn generate(
    catalog: &PgCatalog,
    mapper: &TypeMapper,
    kind: Kind,
    text: &str,
    roles: &[String],
) -> Result<Generated, GenError> {
    mapper.used.borrow_mut().clear();
    let mut g = match kind {
        Kind::Query => {
            let analyzed = if roles.is_empty() {
                catalog.analyze_located(text)?
            } else {
                let mut analyses = Vec::with_capacity(roles.len());
                for role in roles {
                    analyses.push((role.as_str(), catalog.analyze_as_located(text, role)?));
                }
                let (first_role, first) = &analyses[0];
                for (role, analysis) in &analyses[1..] {
                    if let Some(difference) = first_difference(first, analysis) {
                        return Err(GenError::plain(difference.message(first_role, role)));
                    }
                }
                analyses.remove(0).1
            };
            generate_query(&analyzed, mapper)
        }
        Kind::CopyIn => generate_copy_in(&catalog.analyze_copy_in_located(text)?, mapper),
    }?;
    g.overrides = std::mem::take(&mut *mapper.used.borrow_mut())
        .into_iter()
        .collect();
    Ok(g)
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
            "positional placeholder `${}` is not supported: name the parameter (e.g. `$id`) so it \
             can be bound",
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
    // The query's `params` object has one property per name: a parameter and
    // a spread can't share one, and neither can two spreads.
    let mut param_names: std::collections::HashSet<&str> =
        a.params.iter().map(|p| p.name.as_str()).collect();
    for spread in &a.spreads {
        if param_names.contains(spread.name.as_str()) {
            return Err(GenError::plain(
                if a.params.iter().any(|p| p.name == spread.name) {
                    format!(
                        "duplicate parameter name \"{}\": a parameter and a spread can't share one \
                     name",
                        spread.name
                    )
                } else {
                    format!(
                        "duplicate spread name \"{}\": two spreads can't share one name",
                        spread.name
                    )
                },
            ));
        }
        param_names.insert(spread.name.as_str());
    }
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
        let ty = match s.kind {
            SpreadKind::Rows => {
                let fields: Vec<String> = s
                    .fields
                    .iter()
                    .map(|f| member(&f.name, &mapper.map(&f.pg_type).input, f.nullable))
                    .collect();
                format!("readonly {{ {} }}[]", fields.join("; "))
            }
            SpreadKind::List => {
                let e = &s.fields[0];
                let element = nullable(&mapper.map(&e.pg_type).input, e.nullable);
                if element.contains('|') {
                    format!("readonly ({element})[]")
                } else {
                    format!("readonly {element}[]")
                }
            }
        };
        params_ty.push(format!("{}: {ty}", property_key(&s.name)));
    }
    let (row_ty, columns_rt) = columns(&a.columns, mapper);
    let mut types = format!(
        "{{\n    params: {};\n    row: {};\n",
        object(&params_ty, 4),
        object(&row_ty, 4)
    );
    let value = match a.columns.as_slice() {
        [c] => Some(nullable(
            &mapper
                .map_column(&c.pg_type, &c.refinement, &c.elem_refinement)
                .output,
            c.nullable,
        )),
        _ => None,
    };
    if let Some(v) = &value {
        let _ = writeln!(types, "    value: {v};");
    }
    types.push_str("  }");

    // ── runtime ──
    let mut spec = serde_json::Map::new();
    let sql = if a.spreads.is_empty() {
        json!([cast_range(a, 0, a.sql.len())])
    } else {
        Value::Array(sql_pieces(a, 0, a.sql.len()))
    };
    spec.insert("sql".into(), sql);
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
                            let mut field = vec![
                                json!(f.name),
                                mapper.map(&f.pg_type).codec.to_json(),
                                json!(cast_suffix(&f.pg_type)),
                            ];
                            // What separates the values in the spread's
                            // array form, when it isn't `,`.
                            let delimiter = mapper.delimiter(&f.pg_type);
                            if delimiter != ',' {
                                field.push(json!(delimiter.to_string()));
                            }
                            Value::Array(field)
                        })
                        .collect();
                    let kind = match s.kind {
                        SpreadKind::Rows => "rows",
                        SpreadKind::List => "list",
                    };
                    json!([s.name, fields, kind])
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
        params: params_ty,
        row: row_ty,
        value,
        overrides: Vec::new(),
        runtime: Value::Object(spec).to_string(),
        simplified: simplified_note(a),
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
        params: Vec::new(),
        row,
        value: None,
        overrides: Vec::new(),
        runtime: json!({ "sql": copy_sql, "columns": columns }).to_string(),
        simplified: None,
    })
}

/// [`Generated::simplified`]: the rules applied and the SQL sent.
fn simplified_note(a: &AnalyzedQuery) -> Option<String> {
    if a.simplifications.is_empty() {
        return None;
    }
    let mut rules: Vec<&str> = a.simplifications.iter().map(|s| s.rule.name()).collect();
    rules.dedup();
    Some(format!(
        "Sent simplified by typedpg ({}), into an equivalent query PostgreSQL plans \
         better:\n\n```sql\n{}\n```",
        rules.join(", "),
        a.sql
    ))
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
        let m = mapper.map_column(&c.pg_type, &c.refinement, &c.elem_refinement);
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
pub fn object(members: &[String], indent: usize) -> String {
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

/// The pieces of `a.sql[start..end]`, as the runtime writes the SQL of a
/// query with spreads (and the Rust side's `spread_sql`): its text
/// (parameters cast), `["list", spread, cast, empty]` for a list spread —
/// with `lead, keyword, op, array cast` after where it has an array form —
/// and `["values", items, empty]` for a VALUES list holding rows spreads,
/// each item `["row", pieces]` or `["rows", spread]`, with the fields' array
/// casts after where it has an `unnest` form.
fn sql_pieces(a: &AnalyzedQuery, start: usize, end: usize) -> Vec<Value> {
    // (where, a list spread's index, or a VALUES list)
    let mut events: Vec<(usize, Result<usize, &ValuesList>)> = a
        .spreads
        .iter()
        .enumerate()
        // A list may end the SQL: its offset is then `end`.
        .filter(|(_, s)| s.kind == SpreadKind::List && (start..=end).contains(&s.offset))
        // With an array form, its piece starts at the IN's operand.
        .map(|(i, s)| (s.array_form.map_or(s.offset, |f| f.open), Ok(i)))
        .chain(
            a.values_lists
                .iter()
                .filter(|l| (start..end).contains(&l.start))
                .map(|l| (l.start, Err(l))),
        )
        .collect();
    events.sort_by_key(|e| e.0);
    let mut pieces = Vec::new();
    let mut cursor = start;
    for (at, event) in events {
        // Inside a VALUES list already written.
        if at < cursor {
            continue;
        }
        pieces.push(json!(cast_range(a, cursor, at)));
        match event {
            Ok(si) => {
                let spread = &a.spreads[si];
                let cast = cast_suffix(&spread.fields[0].pg_type);
                // The empty list PG has no syntax for: `x IN` it is false,
                // `x NOT IN` it true.
                let empty = format!("(SELECT NULL{cast} WHERE false)");
                pieces.push(match spread.array_form {
                    Some(form) => json!([
                        "list",
                        si,
                        cast,
                        empty,
                        cast_range(a, form.open, form.keyword),
                        cast_range(a, form.keyword, spread.offset),
                        if form.negated { "<> ALL" } else { "= ANY" },
                        format!("{cast}[]")
                    ]),
                    None => json!(["list", si, cast, empty]),
                });
                cursor = spread.offset;
            }
            Err(list) => {
                let mut items = Vec::new();
                let mut null_rows = Vec::new();
                for item in &list.items {
                    items.push(match *item {
                        ValuesItem::Row { start, end } => json!(["row", sql_pieces(a, start, end)]),
                        ValuesItem::Spread(si) => {
                            let nulls: Vec<String> = a.spreads[si]
                                .fields
                                .iter()
                                .map(|f| format!("NULL{}", cast_suffix(&f.pg_type)))
                                .collect();
                            null_rows.push(format!("({})", nulls.join(", ")));
                            json!(["rows", si])
                        }
                    });
                }
                // No row: a SELECT of none, its columns typed as the
                // list's would be.
                let empty = format!(
                    "SELECT * FROM (VALUES {}) AS __typedpg_empty WHERE false",
                    null_rows.join(", ")
                );
                pieces.push(match list.items.as_slice() {
                    [ValuesItem::Spread(si)] if list.unnest => {
                        let casts: Vec<String> = a.spreads[*si]
                            .fields
                            .iter()
                            .map(|f| format!("{}[]", cast_suffix(&f.pg_type)))
                            .collect();
                        json!(["values", items, empty, casts])
                    }
                    _ => json!(["values", items, empty]),
                });
                cursor = list.end;
            }
        }
    }
    pieces.push(json!(cast_range(a, cursor, end)));
    pieces
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
    use std::collections::HashMap;

    const DDL: &str = "CREATE TYPE mood AS ENUM ('happy', 'sad');
        CREATE TYPE address AS (street text, num int4);
        CREATE DOMAIN tags_d AS text[];
        CREATE TABLE users (id int4 PRIMARY KEY, name text, big int8 NOT NULL, m mood,
                            tags text[] NOT NULL DEFAULT '{}', prefs jsonb, home address,
                            moods mood[], span int4range, period tstzmultirange,
                            t2 tags_d NOT NULL DEFAULT '{}', homes address[]);
        CREATE ROLE gen_app;
        GRANT SELECT (name) ON users TO gen_app;";

    fn run(kind: Kind, sql: &str) -> Result<Generated, GenError> {
        run_as(kind, sql, &[])
    }

    fn run_as(kind: Kind, sql: &str, roles: &[String]) -> Result<Generated, GenError> {
        run_with_ddl(DDL, kind, sql, roles)
    }

    fn run_with_ddl(
        ddl: &str,
        kind: Kind,
        sql: &str,
        roles: &[String],
    ) -> Result<Generated, GenError> {
        let mut c = PgCatalog::new().unwrap();
        c.apply_migration("0001.sql", ddl).unwrap();
        let overrides = HashMap::new();
        let mapper = TypeMapper {
            overrides: &overrides,
            used: Default::default(),
            catalog: &c,
        };
        generate(&c, &mapper, kind, sql, roles)
    }

    fn ok(sql: &str) -> Generated {
        run(Kind::Query, sql).unwrap()
    }

    fn err(sql: &str) -> GenError {
        run(Kind::Query, sql).unwrap_err()
    }

    #[test]
    fn a_simplified_query_sends_the_rewrite_and_says_so() {
        let g = ok("SELECT id FROM users WHERE big NOT IN (SELECT big FROM users WHERE id > $min)");
        let note = g.simplified.expect("simplified");
        assert!(
            note.starts_with("Sent simplified by typedpg (not_in)"),
            "{note}"
        );
        assert!(note.contains("NOT EXISTS"), "{note}");
        assert!(g.runtime.contains("NOT EXISTS"), "{}", g.runtime);
        // Not simplified: no note.
        assert_eq!(ok("SELECT id FROM users").simplified, None);
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
            "period: typedpg.Range<Temporal.Instant>[] | null;",
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
    fn int2vector_parameters_are_cast_to_int2vector() {
        // Not `int2[]`, which has no cast to `int2vector`; written `1 2 3`.
        let g = ok("SELECT $v::int2vector AS v, $o::oidvector AS o");
        assert!(
            g.runtime.contains(
                "SELECT ($1::pg_catalog.int2vector)::int2vector AS v, \
                 ($2::pg_catalog.oidvector)::oidvector AS o"
            ),
            "{}",
            g.runtime
        );
        assert!(
            g.runtime
                .contains(r#""params":[["v",["spaced","number"]],["o",["spaced","number"]]]"#),
            "{}",
            g.runtime
        );
        assert!(g.types.contains("v: readonly number[];"), "{}", g.types);
        assert!(g.types.contains("v: number[];"), "{}", g.types);
    }

    #[test]
    fn a_box_spread_carries_its_delimiter() {
        let g = ok("SELECT b FROM unnest($bs::box[]) AS b WHERE b IN $..wanted");
        assert!(
            g.runtime
                .contains(r#"["wanted",[["wanted","text","::pg_catalog.box",";"]],"list"]"#),
            "{}",
            g.runtime
        );
        assert!(
            g.runtime.contains(r#"["bs",["array","text",";"]]"#),
            "{}",
            g.runtime
        );
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
    fn arrays_of_known_elements_are_arrays_of_their_values() {
        let g = ok("SELECT ARRAY['a', 'b'] AS a, ARRAY[1, NULL] AS b, ARRAY[name] AS c FROM users");
        assert!(g.types.contains(r#"a: ("a" | "b")[];"#), "{}", g.types);
        assert!(g.types.contains("b: (1 | null)[];"), "{}", g.types);
        assert!(g.types.contains("c: (string | null)[];"), "{}", g.types);
        let g = ok("SELECT array_agg(big) FILTER (WHERE big IN (1, 2)) AS v FROM users");
        assert!(
            g.types.contains("value: (1n | 2n)[] | null;"),
            "{}",
            g.types
        );
    }

    #[test]
    fn equal_to_any_of_an_array_is_one_of_its_elements() {
        let g = ok(
            "SELECT name, big FROM users WHERE name = ANY(ARRAY['a', 'b']) \
                    AND big = ANY('{1,2}')",
        );
        assert!(g.types.contains(r#"name: "a" | "b";"#), "{}", g.types);
        assert!(g.types.contains("big: 1n | 2n;"), "{}", g.types);
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
            r#""spreads":[["rows",[["id","number","::pg_catalog.int4"],["name","text","::pg_catalog.text"],["big","bigint","::pg_catalog.int8"],["tags",["array","text"],"::pg_catalog.text[]"]],"rows"]]"#
        ), "{}", g.runtime);
        assert!(!g.runtime.contains("subquery"));

        // The SQL in pieces: the VALUES list, with what it is with no row.
        assert!(g.runtime.contains(
            r#""sql":["INSERT INTO users (id, name, big, tags) ",["values",[["rows",0]],"SELECT * FROM (VALUES (NULL::pg_catalog.int4, NULL::pg_catalog.text, NULL::pg_catalog.int8, NULL::pg_catalog.text[])) AS __typedpg_empty WHERE false"]," RETURNING id"]"#
        ), "{}", g.runtime);

        // No `unnest` form: it would flatten `tags`, an array.
        assert!(
            g.runtime
                .contains(r#"AS __typedpg_empty WHERE false"]," RETURNING id"]"#),
            "{}",
            g.runtime
        );
        // Without it, the VALUES list has one: its fields' array casts.
        let g =
            ok("INSERT INTO users (id, name, big) VALUES $..rows { id, name, big } RETURNING id");
        assert!(
            g.runtime.contains(
                r#"WHERE false",["::pg_catalog.int4[]","::pg_catalog.text[]","::pg_catalog.int8[]"]]," RETURNING id"]"#
            ),
            "{}",
            g.runtime
        );

        // A list: its one field is the element, typed by the IN's left side.
        let g = ok("SELECT id FROM users WHERE big IN $..bigs AND name NOT IN $..names");
        assert!(
            g.types.contains(
                "bigs: readonly (bigint | number | string)[];\n      names: readonly string[];"
            ),
            "{}",
            g.types
        );
        assert!(g.runtime.contains(
            r#""spreads":[["bigs",[["bigs","bigint","::pg_catalog.int8"]],"list"],["names",[["names","text","::pg_catalog.text"]],"list"]]"#
        ), "{}", g.runtime);
        // Each with its array form: the operand, the keyword, the operator.
        assert!(g.runtime.contains(
            r#"["list",0,"::pg_catalog.int8","(SELECT NULL::pg_catalog.int8 WHERE false)","big ","IN ","= ANY","::pg_catalog.int8[]"]"#
        ), "{}", g.runtime);
        assert!(
            g.runtime
                .contains(r#""name ","NOT IN ","<> ALL","::pg_catalog.text[]"]"#),
            "{}",
            g.runtime
        );
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
    fn role_checks() {
        let app = ["gen_app".to_owned()];
        // The role has SELECT on the one column.
        assert!(run_as(Kind::Query, "SELECT name FROM users", &app).is_ok());
        assert!(
            run_as(Kind::Query, "SELECT big FROM users WHERE name = $n", &app).is_err(),
            "the column the WHERE reads is not granted"
        );
        let e = run_as(Kind::Query, "SELECT id FROM users", &app).unwrap_err();
        assert!(
            e.message.starts_with("permission denied for table users"),
            "{}",
            e.message
        );
        assert!(
            e.message.contains("grant it in a migration"),
            "{}",
            e.message
        );
        // A privilege denial has no source location of its own: a tool
        // reports it at the query.
        assert_eq!(e.offset, None);

        // Only roles the migrations create can be checked.
        let e = run_as(
            Kind::Query,
            "SELECT name FROM users",
            &["nobody".to_owned()],
        )
        .unwrap_err();
        assert!(
            e.message.starts_with("role \"nobody\" does not exist"),
            "{}",
            e.message
        );

        // A COPY target's privileges are not modeled: never checked.
        assert!(
            run_as(Kind::CopyIn, "users (id, name)", &["nobody".to_owned()]).is_ok(),
            "a COPY target is not checked against roles"
        );
    }

    /// Two roles whose `search_path`s make `items` resolve to tables with
    /// different `x` columns.
    const SEARCH_PATHS: &str = "
        CREATE ROLE path_alt;
        CREATE ROLE path_public;
        CREATE SCHEMA alternate;
        CREATE TABLE public.items (id int4 PRIMARY KEY, x int4 NOT NULL);
        CREATE TABLE alternate.items (id int4 PRIMARY KEY, x text NOT NULL);
        GRANT USAGE ON SCHEMA alternate TO path_alt;
        GRANT SELECT ON public.items TO path_alt, path_public;
        GRANT SELECT ON alternate.items TO path_alt;
        ALTER ROLE path_alt SET search_path = alternate, public;
        ALTER ROLE path_public SET search_path = public;
    ";

    /// The generated types are those of the role's own resolution: the
    /// alternate table's `x` is text, the public one's is an integer.
    #[test]
    fn the_role_search_path_decides_the_generated_types() {
        let g = run_with_ddl(
            SEARCH_PATHS,
            Kind::Query,
            "SELECT x FROM items",
            &["path_alt".to_owned()],
        )
        .unwrap();
        assert!(g.types.contains("x: string;"), "{}", g.types);

        let g = run_with_ddl(
            SEARCH_PATHS,
            Kind::Query,
            "SELECT x FROM items",
            &["path_public".to_owned()],
        )
        .unwrap();
        assert!(g.types.contains("x: number;"), "{}", g.types);
    }

    /// One module has one type per query: roles that resolve the same
    /// statement to different types cannot both be described by it.
    #[test]
    fn roles_that_resolve_the_query_differently_are_an_error() {
        let e = run_with_ddl(
            SEARCH_PATHS,
            Kind::Query,
            "SELECT x FROM items",
            &["path_alt".to_owned(), "path_public".to_owned()],
        )
        .unwrap_err();
        assert_eq!(
            e.message,
            "query resolves differently for roles \"path_alt\" and \"path_public\": column \"x\" \
             is text for \"path_alt\" and integer for \"path_public\" (the roles' search_path \
             settings differ)"
        );
        // A disagreement is about the query, not a position in it.
        assert_eq!(e.offset, None);
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
        // `ROW(…) = ANY($rs)` types `$rs` `record[]`, an array of them.
        assert!(
            err("SELECT (id, name) = ANY($rs) FROM users")
                .message
                .contains("anonymous record")
        );
        assert!(
            err("SELECT id FROM users WHERE id IN $..ids AND name = $ids")
                .message
                .contains(
                    "duplicate parameter name \"ids\": a parameter and a spread can't \
                 share one name"
                )
        );
        assert!(
            err("SELECT id FROM users WHERE id IN $..ids AND name IN $..ids")
                .message
                .contains("duplicate spread name \"ids\": two spreads can't share one name")
        );
    }
}
