//! Soundness and fidelity regressions in core expression typing, each
//! reproduced on PostgreSQL 18: casts and coercions running a function
//! that can return NULL, NULLIF over domains, ROW-to-composite casts,
//! multidimensional ARRAY constructors, row comparisons and literal input.

use crate::common::*;

#[track_caller]
fn nullable(db: &PgCatalog, sql: &str) -> Vec<bool> {
    db.analyze(sql)
        .unwrap_or_else(|e| panic!("`{sql}`: {e}"))
        .columns
        .iter()
        .map(|c| c.nullable)
        .collect()
}

/// Whether the elements of the single (array) column `sql` returns can be
/// NULL.
#[track_caller]
fn elements_nullable(db: &PgCatalog, sql: &str) -> Option<bool> {
    let info = db.analyze(sql).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    match &info.columns[0].pg_type {
        Type::Array {
            element_nullable, ..
        } => *element_nullable,
        other => panic!("`{sql}`: not an array: {other:?}"),
    }
}

/// `color` casts from int through a STRICT SQL function and to text
/// (implicitly) through a STRICT PL/pgSQL one, both returning NULL;
/// `b_type` takes `a_type` implicitly through a non-STRICT SQL function
/// returning NULL.
const CASTS: &str = "
    CREATE TYPE color AS ENUM ('red', 'green');
    CREATE FUNCTION int_to_color(int) RETURNS color
        LANGUAGE sql STRICT IMMUTABLE AS $$ SELECT NULL::color $$;
    CREATE CAST (int AS color) WITH FUNCTION int_to_color(int);
    CREATE FUNCTION color_to_text(color) RETURNS text
        LANGUAGE plpgsql STRICT IMMUTABLE AS $$ BEGIN RETURN NULL; END $$;
    CREATE CAST (color AS text) WITH FUNCTION color_to_text(color) AS IMPLICIT;
    CREATE TYPE a_type AS ENUM ('a');
    CREATE TYPE b_type AS ENUM ('b');
    CREATE FUNCTION a_to_b(a_type) RETURNS b_type
        LANGUAGE sql IMMUTABLE AS $$ SELECT NULL::b_type $$;
    CREATE CAST (a_type AS b_type) WITH FUNCTION a_to_b(a_type) AS IMPLICIT;
    CREATE TABLE t (
        id int PRIMARY KEY, a int NOT NULL, c color NOT NULL, x a_type NOT NULL,
        xs a_type[] NOT NULL, s text NOT NULL
    );";

fn casts_db() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(CASTS).unwrap();
    db
}

// ── A cast function in SQL or PL/pgSQL can return NULL, STRICT or not ──────

#[test]
fn a_strict_sql_or_plpgsql_cast_function_can_return_null() {
    let db = casts_db();
    for sql in [
        "SELECT a::color AS v FROM t",
        "SELECT CAST(a AS color) AS v FROM t",
        "SELECT c::text AS v FROM t",
    ] {
        assert_eq!(nullable(&db, sql), [true], "{sql}");
    }
    // Each element is cast through it.
    for sql in [
        "SELECT ARRAY[a]::color[] AS v FROM t",
        "SELECT ARRAY[a::color] AS v FROM t",
    ] {
        assert_eq!(elements_nullable(&db, sql), Some(true), "{sql}");
    }
    // Near miss: no function runs for an enum label.
    assert_eq!(nullable(&db, "SELECT 'red'::color AS v"), [false]);
}

#[test]
fn a_strict_compiled_extension_cast_function_is_null_only_on_null() {
    // hstore's `hstore_to_json` is a STRICT C function.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE EXTENSION hstore; CREATE TABLE th (h hstore NOT NULL);")
        .unwrap();
    assert_eq!(nullable(&db, "SELECT h::json AS v FROM th"), [false]);
}

// ── Implicit coercions of arguments and branches ───────────────────────────

#[test]
fn a_function_argument_coerced_through_a_cast_function_can_be_null() {
    let db = casts_db();
    for sql in [
        "SELECT upper(c) AS v FROM t",
        "SELECT length(c) AS v FROM t",
        "SELECT string_agg(c, ',') AS v FROM t GROUP BY id",
        // A function in FROM.
        "SELECT u FROM t, upper(c) AS u",
    ] {
        assert_eq!(nullable(&db, sql), [true], "{sql}");
    }
    // `array_append(anycompatiblearray, anycompatible)` coerces the array
    // element by element.
    assert_eq!(
        elements_nullable(&db, "SELECT array_append(xs, 'b'::b_type) AS v FROM t"),
        Some(true)
    );
    // Near miss: no coercion.
    assert_eq!(nullable(&db, "SELECT upper(s) AS v FROM t"), [false]);
}

#[test]
fn an_aggregate_input_proven_present_can_still_be_coerced_to_null() {
    // `HAVING count(ynn) > 0` proves a non-NULL `ynn` in the group — but
    // `sum` reads it cast to int, NULL.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TYPE yn AS ENUM ('y', 'n');
         CREATE FUNCTION yn_int(yn) RETURNS int
             LANGUAGE sql STRICT IMMUTABLE AS 'SELECT NULL::int';
         CREATE CAST (yn AS int) WITH FUNCTION yn_int(yn) AS IMPLICIT;
         CREATE TABLE ty (id int PRIMARY KEY, i int, ynn yn);",
    )
    .unwrap();
    assert_eq!(
        nullable(
            &db,
            "SELECT sum(ynn) AS v FROM ty GROUP BY id HAVING count(ynn) > 0"
        ),
        [true]
    );
    // Near miss: an uncoerced input proven present.
    assert_eq!(
        nullable(
            &db,
            "SELECT sum(i) AS v FROM ty GROUP BY id HAVING count(i) > 0"
        ),
        [false]
    );
}

#[test]
fn an_operand_coerced_through_a_cast_function_can_be_null() {
    let db = casts_db();
    for sql in [
        "SELECT c = 'red'::text AS v FROM t",
        "SELECT c LIKE 'r%' AS v FROM t",
        "SELECT c BETWEEN 'a'::text AND 'z'::text AS v FROM t",
        "SELECT c = ANY(ARRAY['red'::text]) AS v FROM t",
        "SELECT c = ALL('{red}'::text[]) AS v FROM t",
        "SELECT c IN ('red'::text, 'x'::text) AS v FROM t",
        "SELECT c IN (SELECT 'red'::text) AS v FROM t",
        "SELECT ROW(c, 1) = ROW('red'::text, 1) AS v FROM t",
    ] {
        assert_eq!(nullable(&db, sql), [true], "{sql}");
    }
    // `||` on arrays coerces each side's elements.
    assert_eq!(
        elements_nullable(&db, "SELECT ARRAY[x] || ARRAY['b'::b_type] AS v FROM t"),
        Some(true)
    );
    // Near misses: no coercion; IS DISTINCT FROM is NULL only when `=` is.
    for sql in [
        "SELECT s = 'red' AS v FROM t",
        "SELECT c IS DISTINCT FROM 'red'::text AS v FROM t",
    ] {
        assert_eq!(nullable(&db, sql), [false], "{sql}");
    }
}

#[test]
fn a_comparison_whose_operator_returns_null_makes_in_and_distinct_null() {
    // A user `=` in SQL returning NULL: `IN` and `IS DISTINCT FROM` return
    // what it does.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TYPE a_type AS ENUM ('a');
         CREATE FUNCTION a_eq(a_type, a_type) RETURNS bool
             LANGUAGE sql STRICT IMMUTABLE AS 'SELECT NULL::bool';
         CREATE OPERATOR = (LEFTARG = a_type, RIGHTARG = a_type, FUNCTION = a_eq);
         CREATE TABLE ta (x a_type NOT NULL);",
    )
    .unwrap();
    for sql in [
        "SELECT x IN ('a'::a_type) AS v FROM ta",
        "SELECT x IN ('a'::a_type, 'a') AS v FROM ta",
        "SELECT x IS DISTINCT FROM 'a'::a_type AS v FROM ta",
        "SELECT x IS NOT DISTINCT FROM 'a'::a_type AS v FROM ta",
        "SELECT ROW(x, 1) IS DISTINCT FROM ROW('a'::a_type, 1) AS v FROM ta",
    ] {
        assert_eq!(nullable(&db, sql), [true], "{sql}");
    }
    // Near miss: a NULL operand is decided without the operator.
    assert_eq!(
        nullable(&db, "SELECT x IS DISTINCT FROM NULL AS v FROM ta"),
        [false]
    );
}

#[test]
fn a_boolean_argument_coerced_through_a_cast_function_can_be_null() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TYPE yn AS ENUM ('y', 'n');
         CREATE FUNCTION yn_bool(yn) RETURNS bool
             LANGUAGE sql STRICT IMMUTABLE AS 'SELECT NULL::bool';
         CREATE CAST (yn AS bool) WITH FUNCTION yn_bool(yn) AS IMPLICIT;
         CREATE TABLE ty (y yn NOT NULL, b bool NOT NULL);",
    )
    .unwrap();
    for sql in ["SELECT (y AND b) AS v FROM ty", "SELECT NOT y AS v FROM ty"] {
        assert_eq!(nullable(&db, sql), [true], "{sql}");
    }
    assert_eq!(nullable(&db, "SELECT (b AND b) AS v FROM ty"), [false]);
}

#[test]
fn a_branch_coerced_to_the_common_type_can_be_null() {
    let db = casts_db();
    for sql in [
        "SELECT COALESCE(x, NULL::b_type) AS v FROM t",
        "SELECT GREATEST(x, NULL::b_type) AS v FROM t",
        "SELECT CASE WHEN true THEN x ELSE 'b'::b_type END AS v FROM t",
        "SELECT CASE WHEN id > 0 THEN x ELSE 'b'::b_type END AS v FROM t",
        "SELECT x AS v FROM t UNION ALL SELECT 'b'::b_type",
        "SELECT x AS v FROM t INTERSECT SELECT NULL::b_type",
        "SELECT v FROM (VALUES ((SELECT x FROM t LIMIT 1)), ('b'::b_type)) s(v)",
        "SELECT v FROM (VALUES ('a'::a_type), ('b'::b_type)) s(v)",
    ] {
        assert_eq!(nullable(&db, sql), [true], "{sql}");
    }
    // An array coerced element by element.
    assert_eq!(
        elements_nullable(
            &db,
            "SELECT xs AS v FROM t UNION ALL SELECT ARRAY['b'::b_type]"
        ),
        Some(true)
    );
    // Near misses: the branches of the common type; a NOT NULL one
    // uncoerced decides GREATEST / COALESCE.
    for sql in [
        "SELECT COALESCE(x, x) AS v FROM t",
        "SELECT GREATEST('b'::b_type, x) AS v FROM t",
        "SELECT COALESCE(NULL::b_type, x, 'b'::b_type) AS v FROM t",
        "SELECT 'b'::b_type AS v UNION ALL SELECT 'b'::b_type",
    ] {
        assert_eq!(nullable(&db, sql), [false], "{sql}");
    }
}

// ── NULLIF over a domain ───────────────────────────────────────────────────

#[test]
fn nullif_over_a_domain_on_an_array_or_range_is_the_base_type() {
    // `coerce_type` relabels a domain to its base type for a pseudo-type
    // input whose value must be a true array / range (anyarray, anyrange).
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE DOMAIN intarr AS int[];
         CREATE DOMAIN drange AS int4range;
         CREATE TABLE u (id int PRIMARY KEY, x int NOT NULL);
         CREATE DOMAIN ud AS u;
         CREATE TABLE td (da intarr NOT NULL, r drange NOT NULL, c ud NOT NULL);",
    )
    .unwrap();
    let ty = |sql: &str| db.analyze(sql).unwrap().columns[0].pg_type.clone();
    assert_eq!(ty("SELECT NULLIF(da, '{}') AS v FROM td"), array_of(int4()));
    assert_eq!(ty("SELECT NULLIF(da, da) AS v FROM td"), array_of(int4()));
    assert_eq!(
        ty("SELECT NULLIF(r, 'empty') AS v FROM td"),
        range_of("pg_catalog", "int4range", int4())
    );
    // Near miss: `record = record` keeps the domain.
    assert!(matches!(
        ty("SELECT NULLIF(c, c) AS v FROM td"),
        Type::Domain { name, .. } if name == "ud"
    ));
}

// ── ROW(…)::composite converts its fields in the explicit context ──────────

#[test]
fn a_row_cast_to_a_composite_converts_fields_explicitly() {
    let mut db = casts_db();
    db.apply_sql(
        "CREATE TYPE bc AS (b bool, j int);
         CREATE TYPE cp AS (c color, s text);
         CREATE TABLE tj (j jsonb NOT NULL, bcc bc);",
    )
    .unwrap();
    // int → bool and jsonb → int are explicit-only casts.
    db.analyze("SELECT ROW(1, 2)::bc AS v").unwrap();
    db.analyze("SELECT ROW(true, j)::bc AS v FROM tj").unwrap();
    // A field converted by a function that can return NULL.
    assert_eq!(
        nullable(&db, "SELECT (ROW(a, 'x')::cp).c AS v FROM t"),
        [true]
    );
    // Near misses: no explicit cast either; a stored value is converted in
    // the assignment context.
    assert_err_prefix!(
        db.analyze("SELECT ROW(1.5, 2)::bc AS v"),
        AnalyzeError::Invalid(_),
        "cannot cast type record to bc"
    );
    assert_err_prefix!(
        db.analyze("INSERT INTO tj (j, bcc) VALUES ('1', ROW(1, 2))"),
        AnalyzeError::Invalid(_),
        "cannot cast type record to bc"
    );
}

// ── Multidimensional ARRAY[…] whose sub-arrays can't agree ─────────────────

#[test]
fn a_multidimensional_array_of_mismatched_sub_arrays_is_rejected() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE tm (a int NOT NULL, arr int[] NOT NULL);")
        .unwrap();
    const MSG: &str =
        "multidimensional arrays must have array expressions with matching dimensions";
    for sql in [
        "SELECT ARRAY[ARRAY[1], NULL] AS v",
        "SELECT ARRAY[ARRAY[1], ARRAY[1, 2]] AS v",
        "SELECT ARRAY[ARRAY[1], ARRAY[]::int[]] AS v",
        "SELECT ARRAY[ARRAY[1], '{}'] AS v",
        "SELECT ARRAY[ARRAY[ARRAY[1]], ARRAY[ARRAY[1, 2]]] AS v",
    ] {
        assert_err_prefix!(db.analyze(sql), AnalyzeError::Invalid(_), MSG);
    }
    // Over the rows of a table it fails for every row (an empty table
    // never evaluates it: the oracle's execute fallback runs on one).
    db.skip_pg_sanity();
    assert_err_prefix!(
        db.analyze("SELECT ARRAY[ARRAY[a], NULL] AS v FROM tm"),
        AnalyzeError::Invalid(_),
        MSG
    );
}

#[test]
fn a_multidimensional_array_of_sub_arrays_that_can_agree_is_accepted() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE tm (a int NOT NULL, arr int[] NOT NULL);")
        .unwrap();
    for sql in [
        // All NULL / empty: an empty array.
        "SELECT ARRAY[NULL, ARRAY[]::int[]] AS v",
        "SELECT ARRAY[ARRAY[a], ARRAY[NULL::int]] AS v FROM tm",
        "SELECT ARRAY[ARRAY[ARRAY[a], ARRAY[a]], ARRAY[ARRAY[a], ARRAY[a]]] AS v FROM tm",
        // `arr` may be of one element.
        "SELECT ARRAY[ARRAY[a], arr] AS v FROM tm",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    }
}

// ── Row comparisons need a btree interpretation ────────────────────────────

#[test]
fn a_row_comparison_needs_btree_operators() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE FUNCTION myeq(int, int) RETURNS bool
             LANGUAGE sql STRICT IMMUTABLE AS 'SELECT $1 = $2';
         CREATE OPERATOR === (LEFTARG = int, RIGHTARG = int, FUNCTION = myeq);
         CREATE TABLE tr (a int NOT NULL, bx box NOT NULL);
         CREATE TABLE ur (x int NOT NULL, y int NOT NULL);",
    )
    .unwrap();
    for (sql, op) in [
        ("SELECT ROW(a, a) === ROW(1, 1) AS v FROM tr", "==="),
        (
            "SELECT (a, a) === ANY (SELECT x, y FROM ur) AS v FROM tr",
            "===",
        ),
        // `box = box` is in no btree family.
        ("SELECT ROW(bx, a) = ROW(bx, 1) AS v FROM tr", "="),
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::FeatureNotSupported(_),
            &format!("could not determine interpretation of row comparison operator {op}")
        );
    }
    for sql in [
        // A single column: the operator alone.
        "SELECT ROW(a) === ROW(1) AS v FROM tr",
        "SELECT ROW(a, a) < ROW(1, 1) AS v FROM tr",
        // `<>` is the negator of a btree equality.
        "SELECT ROW(a, a) <> ROW(1, 1) AS v FROM tr",
        "SELECT (a, a) = ANY (SELECT x, y FROM ur) AS v FROM tr",
        // IS DISTINCT FROM compares column by column.
        "SELECT ROW(bx, a) IS DISTINCT FROM ROW(bx, 1) AS v FROM tr",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    }
}

// ── Literal input ──────────────────────────────────────────────────────────

#[test]
fn literal_input_matches_the_input_functions() {
    let db = PgCatalog::new().unwrap();
    // uuid_in: a hyphen after any group of four digits.
    for sql in [
        "SELECT '{a0eebc99-9c0b4ef8-bb6d6bb9-bd380a11}'::uuid AS v",
        "SELECT 'a0ee-bc99-9c0b-4ef8-bb6d-6bb9-bd38-0a11'::uuid AS v",
        // json_in doesn't de-escape strings.
        r#"SELECT '"\u0000"'::json AS v"#,
        r#"SELECT '"\ud800"'::json AS v"#,
        r#"SELECT '"😀"'::jsonb AS v"#,
        // apply_typmod rounds before checking.
        "SELECT '99.994'::numeric(4,2) AS v",
        "SELECT '1e-5'::numeric(4,2) AS v",
        "SELECT 'NaN'::numeric(4,2) AS v",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    }
    assert_err_prefix!(
        db.analyze("SELECT 'a0eebc99--9c0b4ef8bb6d6bb9bd380a11'::uuid AS v"),
        AnalyzeError::InvalidLiteral(_),
        "invalid input syntax for type uuid"
    );
    // jsonb_in de-escapes them.
    assert_err_prefix!(
        db.analyze(r#"SELECT '"\u0000"'::jsonb AS v"#),
        AnalyzeError::InvalidLiteral(_),
        "unsupported Unicode escape sequence"
    );
    assert_err_prefix!(
        db.analyze(r#"SELECT '"\ud800"'::jsonb AS v"#),
        AnalyzeError::InvalidLiteral(_),
        "invalid input syntax for type json"
    );
}

#[test]
fn a_numeric_literal_cast_beyond_its_typmod_is_rejected() {
    // The typmod coercion fails every execution.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE DOMAIN dnum AS numeric(4,2);").unwrap();
    for (sql, detail) in [
        (
            "SELECT '123.45'::numeric(4,2) AS v",
            "precision 4, scale 2 must round to an absolute value less than 10^2",
        ),
        (
            "SELECT 123.45::numeric(4,2) AS v",
            "precision 4, scale 2 must round to an absolute value less than 10^2",
        ),
        (
            "SELECT '99.995'::numeric(4,2) AS v",
            "precision 4, scale 2 must round to an absolute value less than 10^2",
        ),
        (
            "SELECT '123.45'::dnum AS v",
            "precision 4, scale 2 must round to an absolute value less than 10^2",
        ),
        (
            "SELECT '12345'::numeric(2,-2) AS v",
            "precision 2, scale -2 must round to an absolute value less than 10^4",
        ),
        (
            "SELECT '1.5'::numeric(2,2) AS v",
            "precision 2, scale 2 must round to an absolute value less than 1",
        ),
        (
            "SELECT 'Infinity'::numeric(4,2) AS v",
            "precision 4, scale 2 cannot hold an infinite value",
        ),
    ] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::Invalid(_),
            &format!("numeric field overflow: a field with {detail}")
        );
    }
    // Near miss: a negative scale rounds to tens; its typmod is PG's.
    let info = db.analyze("SELECT '1234'::numeric(2,-2) AS v").unwrap();
    assert_eq!(
        info.columns[0].pg_type,
        basic_with_typmod("pg_catalog", "numeric", 133122)
    );
}
