//! COLLATE clauses — `expr COLLATE "en_US"` in ORDER BY / WHERE /
//! expressions. The analyzer doesn't model collations explicitly, but the
//! `COLLATE` clause must not change the result type or nullability of the
//! underlying expression — so SELECT/WHERE/ORDER BY queries with a COLLATE
//! decoration should analyze as if the collation were absent.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE users (
            id   BIGINT PRIMARY KEY,
            name TEXT NOT NULL,
            nick TEXT
         );",
    )
    .unwrap();
    db
}

// ── COLLATE in projections ──────────────────────────────────────────────────

#[test]
fn collate_in_select_preserves_text_type_and_nullability() {
    let db = setup();
    // `name COLLATE "C"` is still text, NOT NULL, with the collation
    // surfaced on the output column (PG does the same in the row
    // description).
    let s = db
        .analyze("SELECT name COLLATE \"C\" AS n FROM users")
        .unwrap();
    assert_cols(
        &s,
        vec![c("n", basic_with_collation("pg_catalog", "text", "C"))],
    );
}

#[test]
fn collate_in_select_keeps_nullable() {
    let db = setup();
    let s = db
        .analyze("SELECT nick COLLATE \"C\" AS n FROM users")
        .unwrap();
    assert_cols(
        &s,
        vec![cn("n", basic_with_collation("pg_catalog", "text", "C"))],
    );
}

// ── COLLATE in ORDER BY ─────────────────────────────────────────────────────

#[test]
fn collate_in_order_by_does_not_affect_columns() {
    let db = setup();
    // ORDER BY decorations are invisible at the projection level.
    let s = db
        .analyze("SELECT id, name FROM users ORDER BY name COLLATE \"C\"")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
}

// ── COLLATE in WHERE ────────────────────────────────────────────────────────

#[test]
fn collate_in_where_against_param() {
    let db = setup();
    // `name COLLATE "C" = $p1` — the comparison is still text=text, so the
    // param should be inferred as text.
    let s = db
        .analyze("SELECT id FROM users WHERE name COLLATE \"C\" = $p1")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(text())]);
}

// ── COLLATE on a non-string type must error (PG: collations are not
// supported by type X). Marked ignored: analyzer does not track collation
// applicability today. ──────────────────────────────────────────────────────

#[test]
fn collate_on_int_column_is_rejected() {
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT id COLLATE \"C\" FROM users"),
        AnalyzeError::Invalid(_),
        concat!(
            "collations are not supported by type bigint\n",
            "  ╭────\n",
            "1 │ SELECT id COLLATE \"C\" FROM users\n",
            "  ·        ─┬\n",
            "  ·         ╰─ this is bigint, not a collatable type\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn collate_on_jsonb_column_is_rejected() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (id BIGINT PRIMARY KEY, meta JSONB NOT NULL);")
        .unwrap();
    assert_analyze_err!(
        db.analyze("SELECT meta COLLATE \"C\" FROM t"),
        AnalyzeError::Invalid(_),
        concat!(
            "collations are not supported by type jsonb\n",
            "  ╭────\n",
            "1 │ SELECT meta COLLATE \"C\" FROM t\n",
            "  ·        ──┬─\n",
            "  ·          ╰─ this is jsonb, not a collatable type\n",
            "  ╰────\n",
        ),
    );
}

#[test]
fn collate_on_text_domain_accepted() {
    // A domain over text should still inherit the string category — the
    // analyzer must unwrap the domain before checking applicability.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE DOMAIN slug AS TEXT;
         CREATE TABLE t (id BIGINT PRIMARY KEY, name slug NOT NULL);",
    )
    .unwrap();
    let s = db.analyze("SELECT name COLLATE \"C\" AS n FROM t").unwrap();
    assert_cols(
        &s,
        vec![c("n", domain_with_collation("public", "slug", text(), "C"))],
    );
}

// ── Stacked / nested COLLATE ────────────────────────────────────────────────

#[test]
fn collate_in_concat_expression() {
    let db = setup();
    // `name || (nick COLLATE "C")` — collate on the nullable side; the
    // concat is strict so the result is nullable, and the explicit
    // collation is the operator result's (PG's assign_collations).
    let s = db
        .analyze("SELECT name || (nick COLLATE \"C\") AS combined FROM users")
        .unwrap();
    assert_cols(
        &s,
        vec![cn(
            "combined",
            basic_with_collation("pg_catalog", "text", "C"),
        )],
    );
}

#[test]
fn collate_in_case_branch() {
    let db = setup();
    let s = db
        .analyze("SELECT CASE WHEN id > 0 THEN name COLLATE \"C\" ELSE 'x' END AS v FROM users")
        .unwrap();
    // The branch's explicit collation is the CASE result's.
    assert_cols(
        &s,
        vec![c("v", basic_with_collation("pg_catalog", "text", "C"))],
    );
}

// ── Collation registry — `pg_collation` / `attcollation` not modeled ────────
//
// The analyzer accepts any collation name in a `COLLATE "x"` decoration —
// there's no `pg_collation` to validate against, and no `attcollation`
// recording the column's default collation. PG rejects unknown collations
// up front and propagates a column's collation through expressions.

#[test]
fn collate_unknown_collation_should_error() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT NOT NULL);")
        .unwrap();
    // PG: `collation "definitely_not_a_real_collation" for encoding "UTF8"
    // does not exist`. CREATE TABLE with a bogus column-level COLLATE
    // raises this at apply time; we mirror it.
    let result = db.apply_sql(
        "CREATE TABLE t (
                id BIGINT PRIMARY KEY,
                name TEXT COLLATE \"definitely_not_a_real_collation\" NOT NULL
             );",
    );
    assert_ddl_err!(
        result,
        DdlError::Parse(_),
        "collation \"definitely_not_a_real_collation\" for encoding \"UTF8\" does not exist"
    );
}

#[test]
fn create_collation_then_use_it() {
    // PG: `CREATE COLLATION my_coll (LOCALE = 'C')` registers a new
    // collation in pg_collation. Subsequent `COLLATE "my_coll"` resolves.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE COLLATION my_coll (LOCALE = 'C');
         CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT NOT NULL);",
    )
    .unwrap();
    db.analyze("SELECT name COLLATE \"my_coll\" AS n FROM t")
        .unwrap();
}

#[test]
fn create_collation_from_existing() {
    // PG: `CREATE COLLATION new FROM existing` clones an existing entry.
    // The new row resolves with the same encoding semantics as its source.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE COLLATION my_c FROM \"C\";").unwrap();
    let resolved = db
        .resolve_collation(None, "my_c")
        .expect("clone should be registered");
    let source = db.resolve_collation(None, "C").unwrap();
    assert_eq!(resolved.collencoding, source.collencoding);
}

#[test]
fn create_collation_from_unknown_errors() {
    let mut db = PgCatalog::new().unwrap();
    let result = db.apply_sql("CREATE COLLATION my_c FROM \"definitely_not_a_real_one\";");
    assert_ddl_err!(
        result,
        DdlError::DependencyError(_),
        "collation \"definitely_not_a_real_one\" for encoding \"UTF8\" does not exist"
    );
}

#[test]
fn create_collation_duplicate_name_errors() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE COLLATION my_c (LOCALE = 'C');")
        .unwrap();
    let result = db.apply_sql("CREATE COLLATION my_c (LOCALE = 'C');");
    assert_ddl_err!(
        result,
        DdlError::DuplicateObject(_),
        "collation \"my_c\" for encoding \"UTF8\" already exists"
    );
}

#[test]
fn create_collation_if_not_exists_swallows_duplicate() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE COLLATION my_c (LOCALE = 'C');
         CREATE COLLATION IF NOT EXISTS my_c (LOCALE = 'C');",
    )
    .unwrap();
}

#[test]
fn column_level_collate_in_create_table_is_preserved() {
    // PG: `name TEXT COLLATE "C"` pins the column's default collation in
    // pg_attribute.attcollation. The analyzer now records it, so two
    // tables with different declared collations no longer look identical.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (
            id   BIGINT PRIMARY KEY,
            name TEXT COLLATE \"C\" NOT NULL
         );",
    )
    .unwrap();
    let table = db.resolve_table(None, "t").unwrap();
    let attrs = db.attributes_of(table.oid);
    let name = attrs.iter().find(|a| a.attname == "name").unwrap();
    let c_oid = db
        .resolve_collation(None, "C")
        .expect("\"C\" collation must be in the seed")
        .oid;
    assert_eq!(name.attcollation, Some(c_oid));
}

#[test]
fn collate_on_array_of_collatable_element_accepted() {
    // Arrays of collatable elements are collatable (the collation applies
    // element-wise): `tags COLLATE "C"` is valid; `nums COLLATE "C"` is
    // not, and the message names the array type.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE c2 (tags TEXT[], nums INT[]);")
        .unwrap();
    db.analyze("SELECT tags COLLATE \"C\" FROM c2").unwrap();
    let err = db.analyze("SELECT nums COLLATE \"C\" FROM c2").unwrap_err();
    assert!(
        err.to_string()
            .starts_with("collations are not supported by type integer[]"),
        "got: {err}"
    );
}

// ── Collation derivation (assign_collations / merge_collation_state) ───────

fn collation_db() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE tc (s text NOT NULL, sn text, sc text COLLATE \"C\", \
         sp text COLLATE \"POSIX\", n int);",
    )
    .unwrap();
    db
}

#[test]
fn conflicting_explicit_collations_rejected() {
    let db = collation_db();
    for sql in [
        "SELECT s COLLATE \"C\" < sn COLLATE \"POSIX\" FROM tc",
        "SELECT s COLLATE \"C\" = 'x' COLLATE \"POSIX\" FROM tc",
        "SELECT sc COLLATE \"C\" || sp COLLATE \"POSIX\" FROM tc",
        "SELECT upper(s COLLATE \"C\") = s COLLATE \"POSIX\" FROM tc",
        "SELECT CASE WHEN n > 0 THEN s COLLATE \"C\" ELSE s COLLATE \"POSIX\" END FROM tc",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::CollationMismatch(_),
            "collation mismatch between explicit collations \"C\" and \"POSIX\""
        );
    }
}

#[test]
fn collation_flows_through_functions_operators_and_constructs() {
    // attcollation of the equivalent view columns on PG 18.
    let db = collation_db();
    let s = db
        .analyze(
            "SELECT upper(s COLLATE \"C\") AS a, s COLLATE \"C\" || 'x' AS b, sc AS c, \
             upper(sc) AS d, sc || s AS e, COALESCE(sc, s) AS f, \
             CASE WHEN n > 0 THEN sc ELSE s END AS g, (s COLLATE \"C\")::varchar AS h, \
             n::text AS i, NULLIF(sc, 'x') AS k, (sc || sp) COLLATE \"C\" AS m, \
             GREATEST(sc, s) AS o, sc COLLATE \"POSIX\" AS p, lower(sp) AS q FROM tc",
        )
        .unwrap();
    let got: Vec<(String, Option<String>)> = s
        .columns
        .iter()
        .map(|c| {
            let coll = match &c.pg_type {
                Type::Basic { collation, .. } => collation.clone(),
                _ => None,
            };
            (c.name.clone(), coll)
        })
        .collect();
    let want = |n: &str, c: Option<&str>| (n.to_string(), c.map(str::to_string));
    assert_eq!(
        got,
        vec![
            want("a", Some("C")),
            want("b", Some("C")),
            want("c", Some("C")),
            want("d", Some("C")),
            want("e", Some("C")),
            want("f", Some("C")),
            want("g", Some("C")),
            want("h", Some("C")),
            want("i", None),
            want("k", Some("C")),
            want("m", Some("C")),
            want("o", Some("C")),
            want("p", Some("POSIX")),
            want("q", Some("POSIX")),
        ]
    );
}

/// assign_collations: a sort / group key whose collation is indeterminate
/// (two different implicit collations meet) is 42P21 — ORDER BY, GROUP BY,
/// DISTINCT, an aggregate's ORDER BY, and a set operation's columns
/// (except UNION ALL's, which are never compared).
#[test]
fn indeterminate_collation_rejected_for_sort_and_group_keys() {
    let db = collation_db();
    for sql in [
        "SELECT sc FROM tc ORDER BY sc || sp",
        "SELECT DISTINCT sc || sp FROM tc",
        "SELECT sc || sp FROM tc GROUP BY sc || sp",
        "SELECT sc || sp AS x FROM tc GROUP BY 1",
        "SELECT array_agg(sc ORDER BY sc || sp) FROM tc",
        "SELECT sc FROM tc UNION SELECT sp FROM tc",
        "SELECT sc FROM tc INTERSECT SELECT sp FROM tc",
        // A sublink carries its column's collation.
        "SELECT (SELECT sc) || sp FROM tc ORDER BY 1",
        "SELECT ARRAY(SELECT sc FROM tc) || ARRAY[sp] FROM tc ORDER BY 1",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::CollationMismatch(_),
            "collation mismatch between implicit collations \"C\" and \"POSIX\""
        );
    }
    for sql in [
        "SELECT sc || sp FROM tc",
        "SELECT sc FROM tc ORDER BY length(sc || sp)",
        "SELECT sc FROM tc ORDER BY (sc || sp) COLLATE \"C\"",
        "SELECT sc FROM tc ORDER BY sc || s",
        "SELECT sc FROM tc UNION ALL SELECT sp FROM tc",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let s = db
        .analyze("SELECT sc AS x FROM tc UNION ALL SELECT sp FROM tc")
        .unwrap();
    assert_cols(&s, vec![cn("x", text())]);
}

/// select_common_collation: a COLLATE in one arm's select list decides
/// the set operation's column collation; two different ones are 42P21
/// even for UNION ALL.
#[test]
fn set_operation_explicit_collations() {
    let db = collation_db();
    for sql in [
        "SELECT s COLLATE \"C\" FROM tc UNION SELECT sn COLLATE \"POSIX\" FROM tc",
        "SELECT s COLLATE \"C\" FROM tc UNION ALL SELECT sn COLLATE \"POSIX\" FROM tc",
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::CollationMismatch(_),
            "collation mismatch between explicit collations \"C\" and \"POSIX\""
        );
    }
    let s = db
        .analyze("SELECT sp COLLATE \"C\" AS x FROM tc UNION SELECT sp FROM tc")
        .unwrap();
    assert_cols(
        &s,
        vec![cn("x", basic_with_collation("pg_catalog", "text", "C"))],
    );
}
