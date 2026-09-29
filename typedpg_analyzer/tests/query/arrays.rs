//! Array operations: constructor, element access, slicing, length /
//! cardinality, concatenation, `ANY`/`ALL` on arrays, `unnest` in the
//! projection (including the multi-array variadic form in `FROM`),
//! `array_position` / `array_length` return types, and multi-dimensional
//! array constructors / chained subscripts.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE users (
            id    BIGINT PRIMARY KEY,
            tags  TEXT[],
            nums  INT[] NOT NULL
         );",
    )
    .unwrap();
    db
}

// ── ARRAY[...] constructor ───────────────────────────────────────────────────

#[test]
fn array_constructor_int() {
    let db = setup();
    let s = db.analyze("SELECT ARRAY[1, 2, 3] AS a").unwrap();
    assert_cols(&s, vec![c("a", array_of(int4()))]);
}

#[test]
fn array_constructor_text() {
    let db = setup();
    let s = db
        .analyze("SELECT ARRAY['a', 'b', 'c']::text[] AS a")
        .unwrap();
    assert_cols(&s, vec![c("a", array_of(text()))]);
}

// ── Element access `arr[i]` ──────────────────────────────────────────────────

#[test]
fn array_element_access() {
    let db = setup();
    // Subscripting is always nullable (index may be out of bounds).
    let s = db
        .analyze("SELECT tags[1] AS first_tag FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("first_tag", text())]);
}

#[test]
fn array_element_access_not_null_array() {
    let db = setup();
    // Even when the array column itself is NOT NULL, the element is nullable.
    let s = db.analyze("SELECT nums[1] AS first FROM users").unwrap();
    assert_cols(&s, vec![cn("first", int4())]);
}

#[test]
fn array_element_access_implicit_name_is_array_column() {
    let db = setup();
    // PG's FigureColname rule: a pure subscript inherits the subscripted
    // array's name, so `tags[1]` (no alias) is named `tags`, not `?column?`.
    let s = db.analyze("SELECT tags[1] FROM users").unwrap();
    assert_cols(&s, vec![cn("tags", text())]);
}

// ── Slice `arr[lo:hi]` ───────────────────────────────────────────────────────

#[test]
fn array_slice_returns_same_array_type() {
    let db = setup();
    // `nums[1:3]` on an INT[] NOT NULL column with non-null int literal
    // bounds stays an INT[] NOT NULL (out-of-range → empty array, still
    // non-null).
    let s = db.analyze("SELECT nums[1:3] AS slice FROM users").unwrap();
    assert_cols(&s, vec![c("slice", array_of(int4()))]);
}

#[test]
fn array_slice_of_nullable_array_is_nullable() {
    let db = setup();
    let s = db.analyze("SELECT tags[1:3] AS slice FROM users").unwrap();
    assert_cols(&s, vec![cn("slice", array_of(text()))]);
}

#[test]
fn array_slice_bounds_can_be_params() {
    let db = setup();
    // Params inside the slice bounds get an int4 goal — no fallback to text.
    let s = db
        .analyze("SELECT nums[$p1:$p2] AS slice FROM users")
        .unwrap();
    assert_cols(&s, vec![c("slice", array_of(int4()))]);
    assert_params(&s, vec![p(int4()), p(int4())]);
}

#[test]
fn array_subscript_param_is_int4() {
    let db = setup();
    // `nums[$1]` also routes the param through the int4 goal.
    let s = db.analyze("SELECT nums[$p1] AS first FROM users").unwrap();
    assert_cols(&s, vec![cn("first", int4())]);
    assert_params(&s, vec![p(int4())]);
}

// ── ANY / = ANY on an array ──────────────────────────────────────────────────

#[test]
fn any_array_in_where() {
    let db = setup();
    let s = db
        .analyze("SELECT id FROM users WHERE 'x' = ANY(tags)")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![]);
}

#[test]
fn eq_any_param_array() {
    let db = setup();
    // `col = ANY($p1)` — param is inferred as `int4[]` from the int4 column.
    let s = db
        .analyze("SELECT id FROM users WHERE id = ANY($p1)")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
    assert_params(&s, vec![p(array_of(int8()))]);
}

/// `x op ANY/ALL (array)` is NULL when no element decides the result and
/// some element is NULL (`3 = ANY('{1,NULL}')`), so it is NOT NULL only
/// when the operands are and the elements provably are too.
#[test]
fn any_all_nullable_through_null_elements() {
    let db = setup();
    let s = db
        .analyze(
            "SELECT 3 = ANY(nums) AS a, 3 <> ALL(nums) AS b, \
                    id = ANY('{1,NULL}'::int8[]) AS c, id = ANY(ARRAY[1, NULL]) AS d, \
                    id = ANY($arr) AS e, \
                    id = ANY(ARRAY[1, 2]) AS f, id <> ALL(ARRAY[[1], [2]]::int8[]) AS g, \
                    id = ANY(ARRAY[nums[1]]) AS h \
             FROM users",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![
            cn("a", bool_ty()),
            cn("b", bool_ty()),
            cn("c", bool_ty()),
            cn("d", bool_ty()),
            cn("e", bool_ty()),
            c("f", bool_ty()),
            c("g", bool_ty()),
            cn("h", bool_ty()),
        ],
    );
}

// ── Array concatenation `||` ─────────────────────────────────────────────────

#[test]
fn array_concat_text_arrays() {
    let db = setup();
    // `tags` is nullable → concat is nullable (|| is strict).
    let s = db
        .analyze("SELECT tags || ARRAY['z']::text[] AS combined FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("combined", array_of(text()))]);
}

#[test]
fn array_concat_int_arrays_preserves_not_null() {
    let db = setup();
    // `nums` is NOT NULL → concat stays NOT NULL.
    let s = db
        .analyze("SELECT nums || ARRAY[0] AS combined FROM users")
        .unwrap();
    assert_cols(&s, vec![c("combined", array_of(int4()))]);
}

#[test]
fn array_concat_element_to_array() {
    let db = setup();
    // `element || array` also resolves to the array type.
    let s = db
        .analyze("SELECT 0 || nums AS combined FROM users")
        .unwrap();
    assert_cols(&s, vec![c("combined", array_of(int4()))]);
}

// ── array_length / cardinality / array_position ──────────────────────────────

#[test]
fn array_length_returns_int4() {
    let db = setup();
    // `array_length` returns NULL for empty arrays, so it is always nullable.
    let s = db
        .analyze("SELECT array_length(nums, 1) AS n FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("n", int4())]);
}

#[test]
fn cardinality_returns_int4() {
    let db = setup();
    // `cardinality(nums)` — nums is NOT NULL and strict, so the result is
    // NOT NULL (unlike `array_length`, which is defined to return NULL for
    // empty arrays and is therefore always nullable).
    let s = db
        .analyze("SELECT cardinality(nums) AS n FROM users")
        .unwrap();
    assert_cols(&s, vec![c("n", int4())]);
}

#[test]
fn array_position_returns_int4() {
    let db = setup();
    let s = db
        .analyze("SELECT array_position(tags, 'x') AS pos FROM users")
        .unwrap();
    assert_cols(&s, vec![cn("pos", int4())]);
}

// ── unnest in the projection ─────────────────────────────────────────────────

#[test]
fn unnest_in_select_list_text() {
    let db = setup();
    let s = db.analyze("SELECT unnest(tags) AS t FROM users").unwrap();
    assert_cols(&s, vec![cn("t", text())]);
}

#[test]
fn unnest_in_select_list_int() {
    let db = setup();
    let s = db.analyze("SELECT unnest(nums) AS n FROM users").unwrap();
    // `nums` is NOT NULL, but its elements may still be NULL (PG 18:
    // `unnest('{1,NULL}'::int[]) IS NULL` is true for the second row).
    assert_cols(&s, vec![cn("n", int4())]);
}

// ── Slice extras ─────────────────────────────────────────────────────────────

#[test]
fn array_slice_open_lower_bound() {
    let db = setup();
    // `nums[:3]` — PG accepts an omitted lower bound (defaults to 1). The
    // analyzer should preserve the array type.
    let s = db.analyze("SELECT nums[:3] AS slice FROM users").unwrap();
    assert_cols(&s, vec![c("slice", array_of(int4()))]);
}

#[test]
fn array_slice_open_upper_bound() {
    let db = setup();
    // `nums[2:]` — omitted upper bound (defaults to array length).
    let s = db.analyze("SELECT nums[2:] AS slice FROM users").unwrap();
    assert_cols(&s, vec![c("slice", array_of(int4()))]);
}

// ── Multi-dimensional arrays ────────────────────────────────────────────────

#[test]
fn multi_dim_array_constructor() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE m (id BIGINT PRIMARY KEY, grid INT[][] NOT NULL);")
        .unwrap();
    // PG types `INT[][]` the same as `INT[]` (postgres collapses dimensions
    // in pg_type), and the array literal `ARRAY[ARRAY[1,2], ARRAY[3,4]]` is
    // also `int4[]`. We just want the analyzer to land on the same type.
    let s = db
        .analyze("SELECT ARRAY[ARRAY[1, 2], ARRAY[3, 4]] AS grid")
        .unwrap();
    assert_cols(&s, vec![c("grid", array_of(int4()))]);
}

#[test]
fn multi_dim_subscript_two_levels() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE m (id BIGINT PRIMARY KEY, grid INT[][] NOT NULL);")
        .unwrap();
    // `grid[1][2]` projects an int4. Always nullable (out-of-bounds → NULL).
    let s = db.analyze("SELECT grid[1][2] AS cell FROM m").unwrap();
    assert_cols(&s, vec![cn("cell", int4())]);
}

#[test]
fn three_dim_array_constructor() {
    let db = setup();
    // Triple-nested array literal still collapses to the same array OID.
    let s = db
        .analyze("SELECT ARRAY[ARRAY[ARRAY[1, 2], ARRAY[3, 4]]] AS cube")
        .unwrap();
    assert_cols(&s, vec![c("cube", array_of(int4()))]);
}

#[test]
fn multi_dim_subscript_three_levels() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE c (id BIGINT PRIMARY KEY, cube INT[][][] NOT NULL);")
        .unwrap();
    // Three consecutive subscripts on a 3-dim array reduce all the way down
    // to the element type.
    let s = db.analyze("SELECT cube[1][2][3] AS cell FROM c").unwrap();
    assert_cols(&s, vec![cn("cell", int4())]);
}

#[test]
fn multi_dim_subscript_intermediate_keeps_array() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE m (id BIGINT PRIMARY KEY, grid INT[][] NOT NULL);")
        .unwrap();
    // PG does not actually track the declared dimensions in the type system —
    // any number of subscripts on an array is accepted, with the next-but-last
    // result still typed as the array.  `grid[1]` here keeps the array type
    // because the analyzer cannot know without runtime data whether the row
    // really has 2 dimensions.
    let s = db.analyze("SELECT grid[1] AS row1 FROM m").unwrap();
    assert_cols(&s, vec![cn("row1", int4())]);
}

#[test]
fn multi_dim_array_text_constructor() {
    let db = setup();
    // Same collapsing rule for non-numeric element types.
    let s = db
        .analyze("SELECT ARRAY[ARRAY['a','b'], ARRAY['c','d']]::text[] AS m")
        .unwrap();
    assert_cols(&s, vec![c("m", array_of(text()))]);
}

#[test]
fn multi_dim_subscript_then_field_chain() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TYPE pt AS (x INT, y INT);").unwrap();
    db.apply_sql("CREATE TABLE board (id BIGINT PRIMARY KEY, cells pt[][] NOT NULL);")
        .unwrap();
    // After two subscripts we land on a composite; `.x` walks the field.
    let s = db
        .analyze("SELECT (cells[1][2]).x AS cx FROM board")
        .unwrap();
    assert_cols(&s, vec![cn("cx", int4())]);
}

// ── unnest in FROM ──────────────────────────────────────────────────────────

#[test]
fn unnest_in_from_clause_aliased_column() {
    let db = setup();
    // `FROM unnest(arr) AS t(x)` — PG-supported, the analyzer must register
    // the aliased column `x` in scope and resolve it.
    let s = db
        .analyze("SELECT t.x FROM unnest(ARRAY[1, 2, 3]) AS t(x)")
        .unwrap();
    assert_cols(&s, vec![c("x", int4())]);
}

#[test]
fn unnest_in_from_two_arrays_aligned() {
    let db = setup();
    // `unnest(arr1, arr2)` — multi-array form expands into one column per arg.
    let s = db
        .analyze("SELECT t.a, t.b FROM unnest(ARRAY[1, 2], ARRAY['x'::text, 'y']) AS t(a, b)")
        .unwrap();
    // PG pads the shorter array with NULL, so both columns are nullable.
    assert_cols(&s, vec![cn("a", int4()), cn("b", text())]);
}

#[test]
fn unnest_in_from_with_ordinality() {
    let db = setup();
    // WITH ORDINALITY appends an int8 NOT NULL ordinal column.
    let s = db
        .analyze(
            "SELECT t.x, t.ord \
             FROM unnest(ARRAY[10, 20, 30]) WITH ORDINALITY AS t(x, ord)",
        )
        .unwrap();
    assert_cols(&s, vec![c("x", int4()), c("ord", int8())]);
}

#[test]
fn unnest_of_text_column_in_from() {
    let db = setup();
    // `FROM users u, unnest(u.tags) AS t(tag)` — column reference from
    // outer table requires LATERAL semantics. PG implicitly enables LATERAL
    // for set-returning FROM functions.
    let s = db
        .analyze("SELECT u.id, t.tag FROM users u, unnest(u.tags) AS t(tag)")
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), cn("tag", text())]);
}

#[test]
fn unnest_in_from_three_arrays_aligned() {
    let db = setup();
    // The multi-array form generalises to any arity.
    let s = db
        .analyze(
            "SELECT t.a, t.b, t.c FROM unnest(\
                ARRAY[1, 2], \
                ARRAY['x'::text, 'y'], \
                ARRAY[true, false]\
             ) AS t(a, b, c)",
        )
        .unwrap();
    assert_cols(
        &s,
        vec![cn("a", int4()), cn("b", text()), cn("c", bool_ty())],
    );
}

#[test]
fn unnest_in_from_two_arrays_with_ordinality() {
    let db = setup();
    // ORDINALITY adds a single trailing int8 column shared across all unnested
    // input arrays — not one per input.
    let s = db
        .analyze(
            "SELECT t.a, t.b, t.ord \
             FROM unnest(ARRAY[1, 2], ARRAY['x'::text, 'y']) WITH ORDINALITY AS t(a, b, ord)",
        )
        .unwrap();
    assert_cols(&s, vec![cn("a", int4()), cn("b", text()), c("ord", int8())]);
}

#[test]
fn unnest_in_from_two_columns_lateral() {
    let db = setup();
    // `unnest(u.tags, u.nums)` — both columns from the same outer row are
    // visible thanks to implicit LATERAL on function-call FROM items.
    // Both columns are nullable: the shorter array is padded with NULL,
    // and even the NOT NULL `u.nums` may hold NULL elements.
    let s = db
        .analyze(
            "SELECT u.id, x.t, x.n \
             FROM users u, unnest(u.tags, u.nums) AS x(t, n)",
        )
        .unwrap();
    assert_cols(&s, vec![c("id", int8()), cn("t", text()), cn("n", int4())]);
}

#[test]
fn unnest_in_from_multi_arg_non_array_errors() {
    let db = setup();
    // A scalar argument to multi-arg unnest must surface as a clear error,
    // not silently produce a column of unknown type.
    assert_analyze_err!(
        db.analyze("SELECT * FROM unnest(ARRAY[1, 2], 'oops'::text) AS t(a, b)"),
        AnalyzeError::UndefinedFunction(_),
        concat!(
            "function pg_catalog.unnest(text) does not exist (found 3 candidate(s))\n",
            "  ╭────\n",
            "1 │ SELECT * FROM unnest(ARRAY[1, 2], 'oops'::text) AS t(a, b)\n",
            "  ·               ───┬──\n",
            "  ·                  ╰─ function does not exist\n",
            "  ╰────\n",
            "  help: did you mean \"unnest\"?\n",
        ),
    );
}

#[test]
fn array_ndims_on_scalar_rejected() {
    // `array_ndims(anyarray)` is polymorphic: its lone overload only accepts an
    // array. A scalar `integer` must NOT slip through the single-candidate
    // fallback (which blanket-accepted any pseudo-type parameter).
    // PG: `function array_ndims(integer) does not exist`.
    let db = setup();
    assert_analyze_err!(
        db.analyze("SELECT array_ndims(7)"),
        AnalyzeError::UndefinedFunction(_),
        concat!(
            "function array_ndims(integer) does not exist (found 1 candidate(s))\n",
            "  ╭────\n",
            "1 │ SELECT array_ndims(7)\n",
            "  ·        ─────┬─────\n",
            "  ·             ╰─ function does not exist\n",
            "  ╰────\n",
            "  help: did you mean \"array_ndims\"?\n",
        ),
    );
}

// ── Subscript runs (PG's transformContainerSubscripts) ──────────────────────

fn subscript_db() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE DOMAIN posint AS int CHECK (VALUE > 0);
         CREATE DOMAIN ia AS int[];
         CREATE TABLE t (
            arr int[] NOT NULL,
            vca varchar(5)[] NOT NULL,
            p   posint NOT NULL,
            d   ia NOT NULL
         );",
    )
    .unwrap();
    db
}

#[test]
fn slice_anywhere_in_a_subscript_run_makes_it_a_slice() {
    // One `lo:hi` in a run of subscripts makes every subscript a slice, so
    // the result keeps the array type (verified on PG 18).
    let db = subscript_db();
    let s = db
        .analyze("SELECT arr[1:2][1] AS a, arr[:][1] AS b, arr[1][1:1] AS c FROM t")
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("a", array_of(int4())),
            c("b", array_of(int4())),
            c("c", array_of(int4())),
        ],
    );
}

#[test]
fn subscripts_unwrap_domains() {
    // A domain over an array subscripts as its base type.
    let db = subscript_db();
    let s = db.analyze("SELECT d[1] AS g, d[1:1] AS h FROM t").unwrap();
    assert_cols(&s, vec![cn("g", int4()), c("h", array_of(int4()))]);
}

#[test]
fn subscript_errors_match_pg() {
    let db = subscript_db();
    // A domain over a scalar is reported under its base type.
    assert_err_prefix!(
        db.analyze("SELECT p[1] FROM t"),
        AnalyzeError::DatatypeMismatch(_),
        "cannot subscript type integer because it does not support subscripting"
    );
    assert_err_prefix!(
        db.analyze("SELECT arr[true] FROM t"),
        AnalyzeError::DatatypeMismatch(_),
        "array subscript must have type integer"
    );
    assert_err_prefix!(
        db.analyze("SELECT arr['x'] FROM t"),
        AnalyzeError::InvalidLiteral(_),
        "invalid input syntax for type integer: \"x\""
    );
    assert_err_prefix!(
        db.analyze("SELECT arr[1][1][1][1][1][1][1] FROM t"),
        AnalyzeError::Invalid(_),
        "number of array dimensions (7) exceeds the maximum allowed (6)"
    );
}

#[test]
fn update_array_element_assignment_expects_the_element_type() {
    // `SET arr[1] = …` assigns an element (transformAssignmentIndirection),
    // `SET arr[1:2] = …` a slice of the array type.
    let db = subscript_db();
    db.analyze("UPDATE t SET arr[1] = 5").unwrap();
    db.analyze("UPDATE t SET arr[1:2] = ARRAY[5, 6]").unwrap();
    let s = db.analyze("UPDATE t SET arr[1] = $v").unwrap();
    assert_params(&s, vec![pn(int4())]);
    assert_err_prefix!(
        db.analyze("UPDATE t SET arr[1] = 'x'::text"),
        AnalyzeError::DatatypeMismatch(_),
        "subscripted assignment to \"arr\" requires type integer but expression is of type text"
    );
    assert_err_prefix!(
        db.analyze("UPDATE t SET arr[1:2] = 5"),
        AnalyzeError::DatatypeMismatch(_),
        "subscripted assignment to \"arr\" requires type integer[] but expression is of type integer"
    );
    assert_err_prefix!(
        db.analyze("UPDATE t SET p[1] = 1"),
        AnalyzeError::DatatypeMismatch(_),
        "cannot subscript type integer because it does not support subscripting"
    );
}

// ── hstore subscripts (hstore_subscript_handler) ───────────────────────────

#[test]
fn hstore_subscripts_like_hstore_subscript_transform() {
    // hstore 1.8 sets SUBSCRIPT = hstore_subscript_handler: one text
    // subscript, result text (PG 18).
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE EXTENSION hstore; CREATE TABLE th (h hstore NOT NULL, n int);")
        .unwrap();
    let s = db
        .analyze("SELECT h['a'] AS a, h[1] AS b, h[true] AS c FROM th")
        .unwrap();
    assert_cols(&s, vec![cn("a", text()), cn("b", text()), cn("c", text())]);
    let s = db.analyze("SELECT h[$k] AS a FROM th").unwrap();
    assert_params(&s, vec![p(text())]);
    for sql in ["SELECT h['a':'b'] FROM th", "SELECT h['a']['b'] FROM th"] {
        assert_err_prefix!(
            db.analyze(sql),
            AnalyzeError::FeatureNotSupported(_),
            "hstore allows only one subscript"
        );
    }
    for sql in [
        "UPDATE th SET h['a'] = 'x'",
        "UPDATE th SET h['a'] = 1",
        "UPDATE th SET h['a'] = true",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}
