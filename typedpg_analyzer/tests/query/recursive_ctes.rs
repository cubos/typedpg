//! `WITH RECURSIVE`: self-referential CTEs where the recursive arm sees
//! the CTE's own output. The analyzer registers the seed arm's columns in
//! scope before analyzing the recursive arm, then unifies the two arms'
//! types (mirrors PG's common-type resolution over `UNION ALL`).
//!
//! `SEARCH BREADTH/DEPTH FIRST BY … SET …` and `CYCLE … SET … USING …`
//! add bookkeeping columns (the search order, the cycle mark and path);
//! their types and nullability are checked like any other column, against
//! the pg_sanity mirror too.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE categories (
            id        BIGINT PRIMARY KEY,
            parent_id BIGINT,
            name      TEXT NOT NULL
         );
         CREATE TABLE orgs (
            id        BIGINT PRIMARY KEY,
            parent_id BIGINT,
            name      TEXT NOT NULL
         );",
    )
    .unwrap();
    db
}

// ── Basic counter ────────────────────────────────────────────────────────────

#[test]
fn recursive_counter() {
    let db = setup();
    // Classic integer counter. The recursive arm reads back from `t`, which
    // must be resolvable in scope.
    let s = db
        .analyze(
            "WITH RECURSIVE t(n) AS ( \
                SELECT 1 \
                UNION ALL \
                SELECT n + 1 FROM t WHERE n < 10 \
             ) SELECT n FROM t",
        )
        .unwrap();
    assert_cols(&s, vec![c("n", int4())]);
}

#[test]
fn recursive_counter_with_int8_cast() {
    let db = setup();
    // Seed returns `int4` but the recursive arm pushes to `int8` via cast.
    // PG unifies to `int8`; the analyzer does the same.
    let s = db
        .analyze(
            "WITH RECURSIVE t(n) AS ( \
                SELECT 1::int8 \
                UNION ALL \
                SELECT n + 1 FROM t WHERE n < 100 \
             ) SELECT n FROM t",
        )
        .unwrap();
    assert_cols(&s, vec![c("n", int8())]);
}

// ── Hierarchy traversal ──────────────────────────────────────────────────────

#[test]
fn recursive_category_tree() {
    let db = setup();
    // Walk a parent/child tree, accumulating depth.
    let s = db
        .analyze(
            "WITH RECURSIVE tree(id, name, depth) AS ( \
                SELECT id, name, 0 FROM categories WHERE parent_id IS NULL \
                UNION ALL \
                SELECT c.id, c.name, t.depth + 1 \
                FROM categories c JOIN tree t ON c.parent_id = t.id \
             ) SELECT id, name, depth FROM tree",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![c("id", int8()), c("name", text()), c("depth", int4())],
    );
}

#[test]
fn recursive_with_param_in_seed() {
    let db = setup();
    // `$p1` sits inside the seed's WHERE; the param must be typed as int8
    // to match the column.
    let s = db
        .analyze(
            "WITH RECURSIVE tree(id, name) AS ( \
                SELECT id, name FROM orgs WHERE id = $p1 \
                UNION ALL \
                SELECT o.id, o.name \
                FROM orgs o JOIN tree t ON o.parent_id = t.id \
             ) SELECT id, name FROM tree",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), c("name", text())]);
    assert_params(&s, vec![p(int8())]);
}

// ── Nullability unification ──────────────────────────────────────────────────

#[test]
fn recursive_seed_nullable_column_stays_nullable() {
    let db = setup();
    // `parent_id` is nullable in both arms — the CTE column stays nullable.
    let s = db
        .analyze(
            "WITH RECURSIVE tree(id, parent_id) AS ( \
                SELECT id, parent_id FROM categories WHERE parent_id IS NULL \
                UNION ALL \
                SELECT c.id, c.parent_id \
                FROM categories c JOIN tree t ON c.parent_id = t.id \
             ) SELECT id, parent_id FROM tree",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), cn("parent_id", int8())]);
}

#[test]
fn recursive_one_arm_nullable_propagates() {
    let db = setup();
    // Seed always produces a non-null constant, recursive arm reads a
    // nullable column — the union result is nullable.
    let s = db
        .analyze(
            "WITH RECURSIVE tree(id, label) AS ( \
                SELECT id, name FROM categories WHERE parent_id IS NULL \
                UNION ALL \
                SELECT c.id, c.parent_id::text \
                FROM categories c JOIN tree t ON c.id = t.id \
             ) SELECT id, label FROM tree",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), cn("label", text())]);
}

#[test]
fn recursive_term_null_reaches_other_columns_in_later_iterations() {
    let db = setup();
    // The recursive term reads its own output: `b` turns NULL on the
    // first step, and `a`, copied from `b`, on the next one — and `a`
    // from `b` from `c` two steps later. Both arms are NOT NULL as written
    // against the seed's rows.
    let s = db
        .analyze(
            "WITH RECURSIVE r(a, b, c, k) AS ( \
                SELECT 1, 1, 1, 1 \
                UNION ALL \
                SELECT b, c, NULL::int, k + 1 FROM r WHERE k < 4 \
             ) SELECT a, b, c, k FROM r",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            cn("a", int4()),
            cn("b", int4()),
            cn("c", int4()),
            c("k", int4()),
        ],
    );
}

#[test]
fn recursive_term_null_reaches_elements_and_fields() {
    let db = setup();
    // The same through an array's elements and a record's fields.
    let s = db
        .analyze(
            "WITH RECURSIVE r(a, b, x, k) AS ( \
                SELECT ARRAY[1], ARRAY[1], ROW(1), 1 \
                UNION ALL \
                SELECT b, ARRAY[NULL::int], ROW(NULL::int), k + 1 FROM r WHERE k < 3 \
             ) SELECT a, x FROM r",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("a", array_of(int4())),
            c("x", anon_record(vec![rfn("f1", int4())])),
        ],
    );
    // (assert_cols leaves element nullability out.)
    assert_eq!(col(&s, "a").pg_type, array_with_elems(int4(), true));
}

// ── Non-recursive WITH still picks up aliascolnames ──────────────────────────

#[test]
fn non_recursive_cte_with_column_aliases() {
    let db = setup();
    // `WITH t(renamed) AS (SELECT …)` — even without RECURSIVE, the alias
    // list must rewrite the CTE's column names.
    let s = db
        .analyze(
            "WITH t(renamed) AS (SELECT name FROM categories) \
             SELECT renamed FROM t",
        )
        .unwrap();
    assert_cols(&s, vec![c("renamed", text())]);
}

// ── SEARCH BREADTH/DEPTH FIRST BY … SET … ───────────────────────────────────
//
// Adds a synthetic ordering column populated by PG: a record of the
// depth and the keys for BREADTH FIRST, an array of the key records for
// DEPTH FIRST — never NULL.

#[test]
fn recursive_search_breadth_first_registers_path_column() {
    let db = setup();
    let s = db
        .analyze(
            "WITH RECURSIVE tree(id, parent_id) AS ( \
                SELECT id, parent_id FROM categories WHERE parent_id IS NULL \
                UNION ALL \
                SELECT c.id, c.parent_id \
                FROM categories c JOIN tree t ON c.parent_id = t.id \
             ) SEARCH BREADTH FIRST BY id SET ord \
             SELECT id, parent_id, ord FROM tree",
        )
        .unwrap();
    // PG: `ord` is the synthetic path/order column registered by SEARCH.
    // It surfaces as a non-null array type (records of (depth, id)).
    assert_eq!(
        s.columns
            .iter()
            .find(|c| c.name == "ord")
            .map(|c| c.nullable),
        Some(false)
    );
}

#[test]
fn recursive_search_depth_first_registers_path_column() {
    let db = setup();
    let s = db
        .analyze(
            "WITH RECURSIVE tree(id, parent_id) AS ( \
                SELECT id, parent_id FROM categories WHERE parent_id IS NULL \
                UNION ALL \
                SELECT c.id, c.parent_id \
                FROM categories c JOIN tree t ON c.parent_id = t.id \
             ) SEARCH DEPTH FIRST BY id SET ord \
             SELECT id, parent_id, ord FROM tree",
        )
        .unwrap();
    assert_eq!(
        s.columns
            .iter()
            .find(|c| c.name == "ord")
            .map(|c| c.nullable),
        Some(false)
    );
}

// ── CYCLE … SET … USING … ───────────────────────────────────────────────────
//
// Adds two synthetic columns: the cycle mark (`boolean`, or the type of
// the TO / DEFAULT values) and the `path` array of key records.

#[test]
fn recursive_cycle_clause_registers_columns() {
    let db = setup();
    let s = db
        .analyze(
            "WITH RECURSIVE tree(id, parent_id) AS ( \
                SELECT id, parent_id FROM categories WHERE parent_id IS NULL \
                UNION ALL \
                SELECT c.id, c.parent_id \
                FROM categories c JOIN tree t ON c.parent_id = t.id \
             ) CYCLE id SET is_cycle USING path \
             SELECT id, is_cycle, path FROM tree",
        )
        .unwrap();
    // PG: `is_cycle bool NOT NULL`, `path` array of records — NOT NULL.
    assert_eq!(
        s.columns
            .iter()
            .find(|c| c.name == "is_cycle")
            .map(|c| (c.pg_type.clone(), c.nullable)),
        Some((bool_ty(), false)),
    );
}

#[test]
fn recursive_cycle_clause_with_explicit_mark_values() {
    let db = setup();
    // `CYCLE k SET mark TO 'Y' DEFAULT 'N'` — the mark column type is
    // inferred from the literal. Both `mark` and `path` are NOT NULL.
    let s = db
        .analyze(
            "WITH RECURSIVE tree(id, parent_id) AS ( \
                SELECT id, parent_id FROM categories WHERE parent_id IS NULL \
                UNION ALL \
                SELECT c.id, c.parent_id \
                FROM categories c JOIN tree t ON c.parent_id = t.id \
             ) CYCLE id SET mark TO 'Y' DEFAULT 'N' USING path \
             SELECT id, mark FROM tree",
        )
        .unwrap();
    // The mark column is NOT NULL; we don't pin the exact type because
    // typedpg_pg_query reports the inferred type via cycle_mark_type, which
    // depends on the literals — the important assertions are name+nullability.
    let mark = s.columns.iter().find(|c| c.name == "mark").unwrap();
    assert!(!mark.nullable);
}

/// analyzeCTE compares cycle marks with the mark type's equality operator.
#[test]
fn cycle_mark_type_needs_an_equality_operator() {
    let db = setup();
    assert_err_prefix!(
        db.analyze(
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3) \
             CYCLE n SET is_cycle TO point '(1,1)' DEFAULT point '(0,0)' USING path \
             SELECT * FROM r",
        ),
        AnalyzeError::UndefinedFunction(_),
        "could not identify an equality operator for type point"
    );
}

#[test]
fn recursive_search_breadth_first_with_other_columns_intact() {
    let db = setup();
    // SEARCH must not disturb the user-declared columns (`id`, `parent_id`):
    // they keep their pre-search types and nullability.
    let s = db
        .analyze(
            "WITH RECURSIVE tree(id, parent_id) AS ( \
                SELECT id, parent_id FROM categories WHERE parent_id IS NULL \
                UNION ALL \
                SELECT c.id, c.parent_id \
                FROM categories c JOIN tree t ON c.parent_id = t.id \
             ) SEARCH BREADTH FIRST BY id SET ord \
             SELECT id, parent_id FROM tree",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), cn("parent_id", int8())]);
}

#[test]
fn select_list_errors_come_before_where_errors() {
    // transformSelectStmt resolves the select list before WHERE: with both
    // wrong, PG reports the select list's error.
    let db = setup();
    for sql in [
        "SELECT n + 1 FROM (SELECT true AS n) r WHERE n < 5",
        "WITH RECURSIVE r AS (SELECT true AS n UNION ALL \
         SELECT n + 1 FROM r WHERE n < 5) SELECT * FROM r",
    ] {
        let err = db.analyze(sql).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("operator does not exist: boolean + integer"),
            "{sql}: {err}"
        );
    }
}

#[test]
fn recursive_terms_without_a_common_type_are_rejected() {
    // select_common_type over the two terms fails before the "overall"
    // check can: `UNION types boolean and text cannot be matched` (42804).
    let db = setup();
    let err = db
        .analyze(
            "WITH RECURSIVE r AS (SELECT true AS s, 1 AS n UNION ALL \
             SELECT s || 'x', n + 1 FROM r WHERE n < 5) SELECT s FROM r",
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("UNION types boolean and text cannot be matched"),
        "got: {err}"
    );
    assert!(matches!(err, AnalyzeError::DatatypeMismatch(_)), "{err:?}");
}

#[test]
fn recursive_term_type_must_match_non_recursive_term() {
    // PG fixes the CTE's column types from the non-recursive term; a
    // recursive term that would widen the type errors (42804).
    let db = setup();
    let err = db
        .analyze(
            "WITH RECURSIVE r AS (SELECT 1 AS n UNION ALL \
             SELECT n + 3.14 FROM r WHERE n < 5) SELECT * FROM r",
        )
        .unwrap_err();
    assert!(
        err.to_string().starts_with(
            "recursive query \"r\" column 1 has type integer in non-recursive term but type numeric overall"
        ),
        "got: {err}"
    );
    db.analyze(
        "WITH RECURSIVE r AS (SELECT 1 AS n UNION ALL \
         SELECT n + 1 FROM r WHERE n < 5) SELECT * FROM r",
    )
    .unwrap();
}
