//! What PostgreSQL 18.7 changed in DDL: each case from its regression
//! tests (or the release's commits), as 18.7 answers it.

use crate::common::*;

/// Applies `setup` then `stmt`, which must fail with a message starting
/// with `msg`.
fn rejects(setup: &str, stmt: &str, msg: &str) {
    let err = try_apply(&[("0001.sql", setup), ("0002.sql", stmt)]).expect_err(stmt);
    assert!(
        err.to_string().starts_with(msg),
        "{stmt}\n  expected: {msg}\n  got: {err}"
    );
}

// ── fmgr_sql_validator: every parameter, OUT ones included ─────────────────

#[test]
fn sql_functions_reject_pseudo_type_parameters_of_any_mode() {
    for (stmt, msg) in [
        (
            "CREATE FUNCTION functest_a_4(x \"any\") RETURNS int LANGUAGE sql AS 'SELECT 1';",
            "SQL functions cannot have arguments of type \"any\"",
        ),
        (
            "CREATE FUNCTION functest_a_5(OUT a \"any\", OUT b int) LANGUAGE sql \
             AS 'SELECT 1, 2';",
            "SQL functions cannot have arguments of type \"any\"",
        ),
        (
            "CREATE FUNCTION functest_a_6() RETURNS TABLE(a \"any\", b int) LANGUAGE sql \
             AS 'SELECT 1, 2';",
            "SQL functions cannot have arguments of type \"any\"",
        ),
        // Whether or not the body is checked.
        (
            "SET check_function_bodies = off;
             CREATE FUNCTION functest_a_7(OUT a cstring, OUT b int) LANGUAGE sql \
             AS 'SELECT ''x'', 2';",
            "SQL functions cannot have arguments of type cstring",
        ),
        // record is an output column's type only.
        (
            "CREATE FUNCTION functest_a_11(x record) RETURNS int LANGUAGE sql AS 'SELECT 1';",
            "SQL functions cannot have arguments of type record",
        ),
    ] {
        rejects("SELECT 1;", stmt, msg);
    }
    build_db(&[(
        "0001.sql",
        "CREATE FUNCTION functest_a_8(x anyelement, OUT a anyelement, OUT b int)
           LANGUAGE sql AS 'SELECT $1, 2';
         CREATE FUNCTION functest_a_9(OUT a record, OUT b int) LANGUAGE sql
           AS 'SELECT ROW(1, 2), 3';
         CREATE FUNCTION functest_a_10(OUT a record[], OUT b int) LANGUAGE sql
           AS 'SELECT ARRAY[ROW(1, 2)], 3';",
    )]);
}

// ── compute_return_type: no shell type for a decorated name ────────────────

#[test]
fn c_functions_make_no_shell_type_of_a_decorated_name() {
    for (stmt, msg) in [
        (
            "CREATE FUNCTION bogus_in(cstring) RETURNS bogus_shell(123)
               AS 'regresslib', 'widget_in' LANGUAGE C STRICT IMMUTABLE;",
            "type \"bogus_shell\" does not exist",
        ),
        (
            "CREATE FUNCTION bogus_in(cstring) RETURNS bogus_shell[]
               AS 'regresslib', 'widget_in' LANGUAGE C STRICT IMMUTABLE;",
            "type \"bogus_shell[]\" does not exist",
        ),
        (
            "CREATE TABLE bogus_tbl (col int);
             CREATE FUNCTION bogus_in(cstring) RETURNS bogus_tbl.nonexistent_col%TYPE
               AS 'regresslib', 'widget_in' LANGUAGE C STRICT IMMUTABLE;",
            "column \"nonexistent_col\" of relation \"bogus_tbl\" does not exist",
        ),
        (
            "CREATE FUNCTION bogus_in(cstring) RETURNS nonexistent_schema.bogus_shell
               AS 'regresslib', 'widget_in' LANGUAGE C STRICT IMMUTABLE;",
            "schema \"nonexistent_schema\" does not exist",
        ),
    ] {
        rejects("SELECT 1;", stmt, msg);
    }
}

// ── CreateStatistics: an expression's type is checked as a column's ────────

#[test]
fn statistics_expressions_must_have_a_storable_type() {
    let setup = "CREATE TABLE ext_stats_test (x int, y text);";
    for (stmt, msg) in [
        (
            "CREATE STATISTICS tst ON (row(x, y)) FROM ext_stats_test;",
            "expression has pseudo-type record",
        ),
        (
            "CREATE STATISTICS tst ON (array[row(x, y)]) FROM ext_stats_test;",
            "expression has pseudo-type record[]",
        ),
        (
            "CREATE STATISTICS tst ON (array[x::text::cstring]) FROM ext_stats_test;",
            "expression has pseudo-type cstring",
        ),
        // Among several, too.
        (
            "CREATE STATISTICS tst ON (row(x, y)), x FROM ext_stats_test;",
            "expression has pseudo-type record",
        ),
    ] {
        rejects(setup, stmt, msg);
    }
    // A collatable one with a collation.
    rejects(
        "CREATE TABLE ext_stats_coll (a text COLLATE \"C\", b text COLLATE \"POSIX\");",
        "CREATE STATISTICS tst ON ((a || b)) FROM ext_stats_coll;",
        "no collation was derived for expression with collatable type text",
    );
    build_db(&[(
        "0001.sql",
        "CREATE TABLE ext_stats_test (x int, y text);
         CREATE STATISTICS coll ON ((y || 'x')) FROM ext_stats_test;
         CREATE TYPE pair AS (x int, y text);
         CREATE STATISTICS ok ON (row(x, y)::pair) FROM ext_stats_test;",
    )]);
}

// ── OperatorCreate / UpdateOperatorLinks ───────────────────────────────────

#[test]
fn a_commutator_or_negator_returns_the_operators_type() {
    for (setup, stmt, msg) in [
        (
            "SELECT 1;",
            "CREATE OPERATOR === (leftarg = integer, rightarg = integer, procedure = int4eq,
               commutator = <<);",
            "commutator operator << does not return the same data type as operator ===",
        ),
        (
            "CREATE OPERATOR === (leftarg = integer, rightarg = integer, procedure = int4eq,
               commutator = ===!!!);",
            "CREATE OPERATOR ===!!! (leftarg = integer, rightarg = integer, procedure = int4pl);",
            "commutator operator === does not return the same data type as operator ===!!!",
        ),
        (
            "SELECT 1;",
            "CREATE OPERATOR === (leftarg = integer, rightarg = integer, procedure = int4eq,
               negator = <<);",
            "negator operator << does not return the same data type as operator ===",
        ),
        // The shell's negator link makes it need to be boolean, checked
        // first.
        (
            "CREATE OPERATOR === (leftarg = integer, rightarg = integer, procedure = int4eq,
               negator = ===!!!);",
            "CREATE OPERATOR ===!!! (leftarg = integer, rightarg = integer, procedure = int4pl);",
            "only boolean operators can have negators",
        ),
        // ALTER OPERATOR too.
        (
            "CREATE FUNCTION f_bool(boolean, real) RETURNS boolean
               AS 'SELECT NULL::boolean' LANGUAGE sql IMMUTABLE;
             CREATE OPERATOR @!= (leftarg = boolean, rightarg = real, procedure = f_bool);
             CREATE FUNCTION f_int(real, boolean) RETURNS integer
               AS 'SELECT NULL::integer' LANGUAGE sql IMMUTABLE;
             CREATE OPERATOR @=@ (leftarg = real, rightarg = boolean, procedure = f_int);",
            "ALTER OPERATOR @!=(boolean, real) SET (COMMUTATOR = @=@);",
            "commutator operator @=@ does not return the same data type as operator @!=",
        ),
    ] {
        rejects(setup, stmt, msg);
    }
}

#[test]
fn a_shell_operators_links_are_kept() {
    // A shell's links can't be changed...
    for (setup, stmt, msg) in [
        (
            "CREATE OPERATOR === (leftarg = integer, rightarg = integer, procedure = int4eq,
               commutator = ===!!!);",
            "CREATE OPERATOR ===!!! (leftarg = integer, rightarg = integer, procedure = int4eq,
               commutator = <<);",
            "commutator operator === is already the commutator of operator ===!!!",
        ),
        (
            "CREATE OPERATOR === (leftarg = integer, rightarg = integer, procedure = int4eq,
               negator = ===!!!);",
            "CREATE OPERATOR ===!!! (leftarg = integer, rightarg = integer, procedure = int4eq,
               negator = <<);",
            "negator operator === is already the negator of operator ===!!!",
        ),
        // ... nor name the shell itself as its own negator.
        (
            "CREATE OPERATOR === (leftarg = integer, rightarg = integer, procedure = int4eq,
               negator = !==);",
            "CREATE OPERATOR !== (leftarg = integer, rightarg = integer, procedure = int4ne,
               negator = !==);",
            "operator cannot be its own negator",
        ),
    ] {
        rejects(setup, stmt, msg);
    }
    // ... and stay when the shell is filled in without them.
    let snap = build(&[(
        "0001.sql",
        "CREATE FUNCTION fn_op2(boolean, boolean) RETURNS boolean
           AS 'SELECT NULL::boolean' LANGUAGE sql IMMUTABLE;
         CREATE OPERATOR ===== (leftarg = boolean, rightarg = boolean, procedure = fn_op2,
           commutator = =====, negator = !====, restrict = contsel, join = contjoinsel,
           hashes, merges);
         CREATE OPERATOR !==== (leftarg = boolean, rightarg = boolean, procedure = fn_op2,
           commutator = !====);",
    )]);
    let op = |name: &str| {
        snap.pg_operator()
            .values()
            .find(|o| o.oprname == name)
            .unwrap_or_else(|| panic!("operator {name}"))
    };
    let (eq, ne) = (op("====="), op("!===="));
    assert_eq!(eq.oprcom, Some(eq.oid));
    assert_eq!(eq.oprnegate, Some(ne.oid));
    assert_eq!(ne.oprcom, Some(ne.oid));
    assert_eq!(ne.oprnegate, Some(eq.oid));
}

// ── StoreRelNotNull: no NO INHERIT not-null on a partitioned table ─────────

#[test]
fn a_partitioned_table_copies_no_no_inherit_not_null() {
    rejects(
        "CREATE TABLE noinh_con_copy (a int, b int not null, c int not null no inherit);",
        "CREATE TABLE noinh_con_copy1_parted (LIKE noinh_con_copy INCLUDING ALL)
           PARTITION BY LIST (a);",
        "not-null constraints on partitioned tables cannot be NO INHERIT",
    );
    build_db(&[(
        "0001.sql",
        "CREATE TABLE noinh_con_copy (a int, b int not null, c int not null no inherit);
         ALTER TABLE noinh_con_copy ALTER COLUMN c DROP NOT NULL;
         CREATE TABLE noinh_con_copy1_parted (LIKE noinh_con_copy INCLUDING ALL)
           PARTITION BY LIST (a);",
    )]);
}

// ── CREATE EXTENSION: schema names the script can't use ────────────────────

#[test]
fn extension_scripts_refuse_unsafe_schema_names() {
    rejects(
        "CREATE SCHEMA \"$user\";",
        "CREATE EXTENSION cube SCHEMA \"$user\";",
        "unacceptable extension schema name \"$user\"",
    );
    // A required extension's schema, substituted for @extschema:cube@.
    for (schema, msg) in [
        (
            "a'b",
            "invalid character in extension \"cube\" schema: must not contain any of \"\"$'\\\"",
        ),
        (
            "a--b",
            "invalid string in extension \"cube\" schema: must not contain newline, carriage \
             return, \"--\", \"/*\", or \"*/\"",
        ),
        (
            "x@extschema",
            "invalid string in extension \"cube\" schema: must not contain \"@extowner@\", \
             \"@extschema\", or \"MODULE_PATHNAME\"",
        ),
    ] {
        rejects(
            &format!("CREATE SCHEMA \"{schema}\"; CREATE EXTENSION cube SCHEMA \"{schema}\";"),
            "CREATE EXTENSION earthdistance;",
            msg,
        );
    }
}

// ── find_composite_type_dependencies: stored expressions count ────────────

#[test]
fn a_row_type_in_use_cant_change() {
    // (setup, the ALTER, its error) — alter_column_type.sql, alter_table.sql,
    // generated_stored.sql and generated_virtual.sql of 18.7.
    let cases: &[(&str, &str, &str)] = &[
        (
            "create table at_tab1 (a int, b numeric); create table at_tab2 (x int, y at_tab1);",
            "alter table at_tab1 alter column b type bigint;",
            "cannot alter table \"at_tab1\" because column \"at_tab2.y\" uses its row type",
        ),
        (
            "create table attbl(a numeric); create table atref(b attbl check ((b).a is not null));",
            "alter table attbl alter column a type bigint;",
            "cannot alter table \"attbl\" because column \"atref.b\" uses its row type",
        ),
        (
            "create table attbl(a bytea);
             create function atref_func(p attbl default '(\\x4649584d45)') returns attbl
               immutable language sql AS $$select p;$$;",
            "alter table attbl alter column a type text;",
            "cannot alter table \"attbl\" because function atref_func(attbl) uses its row type",
        ),
        (
            "create table attbl(a bytea);
             create function atref_func() returns attbl
               immutable language sql begin atomic select '(\\x4649584d45)'::attbl; end;",
            "alter table attbl alter column a type text;",
            "cannot alter table \"attbl\" because function atref_func() uses its row type",
        ),
        (
            "create table attbl(a bytea);
             create table atref(b text default '(\\x4649584d45)'::attbl);",
            "alter table attbl alter column a type text;",
            "cannot alter table \"attbl\" because default value for column b of table atref \
             uses its row type",
        ),
        (
            "CREATE TYPE atcomp AS (a bytea);
             CREATE FUNCTION atfunc(int) RETURNS atcomp IMMUTABLE LANGUAGE plpgsql AS $$
               BEGIN RETURN ROW($1); END $$;
             CREATE TABLE atref (c int) PARTITION BY LIST (atfunc(c));
             CREATE TABLE atref_1 PARTITION OF atref FOR VALUES IN ('(\\x4649584d45)'::atcomp);",
            "ALTER TYPE atcomp ALTER ATTRIBUTE a TYPE text CASCADE;",
            "cannot alter type \"atcomp\" because table atref uses it",
        ),
        (
            "create table attbl(a numeric); create table atref(data text);
             create index atref_idx on atref ((('(1)'::attbl).a + length(data)));",
            "alter table attbl alter column a type bigint;",
            "cannot alter table \"attbl\" because index atref_idx uses its row type",
        ),
        (
            "create table attbl(a bytea); create table atref(data text);
             create index atref_idx on atref
               (('(\\x4649584d45)'::attbl = '(\\x4649584d45)'::attbl));",
            "alter table attbl alter column a type text;",
            "cannot alter table \"attbl\" because index atref_idx uses its row type",
        ),
        (
            "CREATE TYPE atcomp AS (a numeric); CREATE TABLE atref (c int);
             CREATE FUNCTION atfunc(numeric) RETURNS atcomp IMMUTABLE LANGUAGE plpgsql
               AS $$BEGIN RETURN ('('||$1||')')::atcomp; END$$;
             CREATE INDEX atref_idx ON atref(atfunc(c));",
            "ALTER TYPE atcomp ALTER ATTRIBUTE a TYPE bigint CASCADE;",
            "cannot alter type \"atcomp\" because column \"atref_idx.atfunc\" uses it",
        ),
        (
            "CREATE TYPE atcomp AS (a int); CREATE TABLE atref (c int, d int);
             CREATE FUNCTION atfunc(int) RETURNS atcomp IMMUTABLE LANGUAGE plpgsql
               AS $$BEGIN RETURN ROW($1); END$$;
             CREATE STATISTICS atstat (mcv) ON atfunc(c), d FROM atref;",
            "ALTER TYPE atcomp ALTER ATTRIBUTE a TYPE text CASCADE;",
            "cannot alter type \"atcomp\" because statistics object atstat uses it",
        ),
        (
            "CREATE TYPE atcomp AS (a bytea);
             CREATE DOMAIN atdomain AS atcomp DEFAULT '(\\x4649584d45)'::atcomp;",
            "ALTER TYPE atcomp ALTER ATTRIBUTE a TYPE text CASCADE;",
            "cannot alter type \"atcomp\" because type atdomain uses it",
        ),
        (
            "CREATE TYPE atcomp AS (a bytea);
             CREATE DOMAIN atdomainarr AS atcomp[] DEFAULT '{(\\x4649584d45)}'::atcomp[];",
            "ALTER TYPE atcomp ALTER ATTRIBUTE a TYPE text CASCADE;",
            "cannot alter type \"atcomp\" because type atdomainarr uses it",
        ),
        (
            "CREATE TYPE atcomp AS (a int);
             CREATE DOMAIN atdomain AS atcomp DEFAULT ROW(333)::atcomp;",
            "ALTER TYPE atcomp ALTER ATTRIBUTE a TYPE text;",
            "cannot alter type \"atcomp\" because type atdomain uses it",
        ),
        (
            "create table at_tab1 (a int, b numeric);
             create table at_tab2 (x int, y text, check((x,y)::at_tab1 = (1,'42')::at_tab1));",
            "alter table at_tab1 alter column b type bigint;",
            "cannot alter table \"at_tab1\" because constraint at_tab2_check on table at_tab2 \
             uses its row type",
        ),
        (
            "create table at_tab1 (a int, b bytea);
             create table at_tab2 (x int, y text,
               check(y::text = '(1,\\x4649584d45)'::at_tab1::text));",
            "alter table at_tab1 alter column b type text;",
            "cannot alter table \"at_tab1\" because constraint at_tab2_y_check on table at_tab2 \
             uses its row type",
        ),
        (
            "create table at_tab1 (a int, b text) partition by list(a);
             create table at_tab2 (x int, y at_tab1);",
            "alter table at_tab1 alter column b type varchar;",
            "cannot alter table \"at_tab1\" because column \"at_tab2.y\" uses its row type",
        ),
        (
            "create table attbl(a bytea);
             create view atrefview as
               select '1' as a, (('(\\x4649584d45)'::attbl = '(\\x4649584d45)'::attbl)) as b;",
            "alter table attbl alter column a type text;",
            "cannot alter table \"attbl\" because rule _RETURN on view atrefview uses its row type",
        ),
        (
            "create table at_gt (a int, x int generated always as (a) virtual);
             create view at_gtv as select '(1,2)'::at_gt as r;",
            "alter table at_gt alter column x type bigint;",
            "cannot alter table \"at_gt\" because rule _RETURN on view at_gtv uses its row type",
        ),
        (
            "create table at_pt (x int, y varchar(10)) partition by range (x);
             create view at_ptv as select '(1,hi)'::at_pt;",
            "alter table at_pt alter column y type varchar(20);",
            "cannot alter table \"at_pt\" because rule _RETURN on view at_ptv uses its row type",
        ),
        (
            "create table at_pt (x int, y varchar(10)) partition by range (x);
             create view at_ptv as select '(1,hi)'::at_pt;",
            "alter table at_pt alter column y type bytea using y::bytea;",
            "cannot alter table \"at_pt\" because rule _RETURN on view at_ptv uses its row type",
        ),
        (
            "create table at_tab1 (a int, b int);
             create table at_ptab (x int, y int) partition by list ((row(x,y)::at_tab1));",
            "alter table at_tab1 alter column b type text;",
            "cannot alter table \"at_tab1\" because table at_ptab uses its row type",
        ),
        (
            "create table at_ptab2 (a int, b int) partition by range ((at_ptab2));
             create table at_ptab2_p partition of at_ptab2 for values from ('(0,0)') to ('(9,9)');",
            "alter table at_ptab2 alter column a type bigint;",
            "cannot alter table \"at_ptab2\" because table at_ptab2 uses its row type",
        ),
        (
            "CREATE TYPE test_type1 AS (a int, b text); CREATE TABLE test_tbl1 (x int, y test_type1);",
            "ALTER TYPE test_type1 ALTER ATTRIBUTE b TYPE varchar;",
            "cannot alter type \"test_type1\" because column \"test_tbl1.y\" uses it",
        ),
        (
            "CREATE TYPE test_type1 AS (a int, b text); CREATE TABLE test_tbl1 (x int, y text);
             CREATE INDEX test_tbl1_idx ON test_tbl1((row(x,y)::test_type1));",
            "ALTER TYPE test_type1 ALTER ATTRIBUTE b TYPE varchar;",
            "cannot alter type \"test_type1\" because column \"test_tbl1_idx.row\" uses it",
        ),
        (
            "CREATE TYPE test_type1 AS (a int, b text); CREATE TABLE test_tbl1 (x int, y text);
             CREATE FUNCTION construct_test_type_arr(x int, y text) RETURNS test_type1[]
               LANGUAGE plpgsql IMMUTABLE
               AS $$ begin return array[row(x, y)::test_type1]; end; $$;
             CREATE INDEX test_tbl1_idx ON test_tbl1(construct_test_type_arr(x, y));",
            "ALTER TYPE test_type1 ALTER ATTRIBUTE b TYPE varchar;",
            "cannot alter type \"test_type1\" because column \
             \"test_tbl1_idx.construct_test_type_arr\" uses it",
        ),
        (
            "CREATE TYPE test_type1 AS (a int, b text); CREATE TABLE test_tbl1 (x int, y text);
             CREATE STATISTICS test_tbl1_st (mcv) ON (row(x,y)::test_type1), x FROM test_tbl1;",
            "ALTER TYPE test_type1 ALTER ATTRIBUTE b TYPE varchar;",
            "cannot alter type \"test_type1\" because statistics object test_tbl1_st uses it",
        ),
        (
            "CREATE TYPE test_type1 AS (a int, b text); CREATE TABLE test_tbl1 (x int, y text);
             CREATE TYPE container_type AS (a int, b test_type1);
             CREATE STATISTICS test_tbl1_st (mcv)
               ON (array[row(x, row(x,y)::test_type1)::container_type]), x FROM test_tbl1;",
            "ALTER TYPE test_type1 ALTER ATTRIBUTE b TYPE varchar;",
            "cannot alter type \"test_type1\" because statistics object test_tbl1_st uses it",
        ),
        (
            "CREATE TABLE attmp (initial int4); CREATE TABLE attmp2 (t text, tbl attmp);",
            "ALTER TABLE attmp ADD COLUMN b int4 DEFAULT 100;",
            "cannot alter table \"attmp\" because column \"attmp2.tbl\" uses its row type",
        ),
        (
            "CREATE TABLE attmp (initial int4); CREATE VIEW attmpv AS SELECT '(1)'::attmp AS c;",
            "ALTER TABLE attmp ADD COLUMN c int DEFAULT 0;",
            "cannot alter table \"attmp\" because rule _RETURN on view attmpv uses its row type",
        ),
        (
            "CREATE TABLE attmp (initial int4); CREATE VIEW attmpv AS SELECT '(1)'::attmp AS c;",
            "alter table attmp add column d int generated always as (initial) virtual;",
            "cannot alter table \"attmp\" because rule _RETURN on view attmpv uses its row type",
        ),
        (
            "CREATE TABLE attmp (initial int4); CREATE VIEW attmpv AS SELECT '(1)'::attmp AS c;",
            "alter table attmp add column e int generated always as (initial) stored;",
            "cannot alter table \"attmp\" because rule _RETURN on view attmpv uses its row type",
        ),
        (
            "CREATE TABLE gtest31_1 (a int GENERATED ALWAYS AS (123));
             CREATE TABLE gtest31_2 (b text)
               PARTITION BY RANGE ((('(1)'::gtest31_1).a + length(b)));",
            "ALTER TABLE gtest31_1 ALTER COLUMN a TYPE bigint;",
            "cannot alter table \"gtest31_1\" because table gtest31_2 uses its row type",
        ),
        (
            "CREATE TABLE gtest31_1 (a int GENERATED ALWAYS AS (123) VIRTUAL);
             CREATE TABLE gtest31_2 (b text)
               PARTITION BY RANGE ((('(1)'::gtest31_1).a + length(b)));",
            "ALTER TABLE gtest31_1 ALTER COLUMN a TYPE bigint;",
            "cannot alter table \"gtest31_1\" because table gtest31_2 uses its row type",
        ),
    ];
    for (setup, stmt, msg) in cases {
        rejects(setup, stmt, msg);
    }
}

#[test]
fn a_row_type_merely_typing_something_can_change() {
    build_db(&[(
        "0001.sql",
        // A function taking or returning it, or with a default not using it.
        "create table attbl(a bytea);
         create function atref_func(p text) returns attbl immutable
           language sql AS $$select p::public.attbl;$$;
         create function atref_func2(p attbl) returns text immutable
           language sql AS $$select p::text;$$;
         create function atref_func3(p text default '(\\x4649584d45)') returns attbl
           immutable language sql AS $$select p::public.attbl ;$$;
         alter table attbl alter column a type text;
         -- An index through a function returning it.
         create table attbl2(a numeric); create table atref(data text);
         create function atconst() RETURNS attbl2 LANGUAGE sql IMMUTABLE AS $$
           SELECT '(1)'::public.attbl2; $$;
         create index atref_idx on atref (((atconst()).a + length(data)));
         alter table attbl2 alter column a type bigint;
         -- A domain over it defaulting to NULL.
         CREATE TYPE atcomp AS (a int);
         CREATE DOMAIN atdomain AS atcomp DEFAULT NULL;
         ALTER TYPE atcomp ALTER ATTRIBUTE a TYPE text CASCADE;
         -- Functions, operators and casts using it.
         create table at_tab1 (a int, b text);
         create function at_tab1_eq(at_tab1, at_tab1) returns bool immutable
           language sql as $$ select true $$;
         alter table at_tab1 alter column a type int8;
         create operator === (leftarg = at_tab1, rightarg = at_tab1, function = at_tab1_eq);
         alter table at_tab1 alter column a type int4;
         create function at_tab1_cast(text) returns at_tab1 immutable language sql
           as $$select ('1', $1)::public.at_tab1$$;
         create cast (text as at_tab1) with function at_tab1_cast(text);
         alter table at_tab1 alter column b type varchar;
         -- A view of a function returning it.
         create table at_tab3 (a int, b bytea);
         create function at_tab3_const() returns at_tab3 language plpgsql as $$
           begin return '(1,ok)'::at_tab3; end; $$;
         create view at_view1 as select at_tab3_const();
         alter table at_tab3 alter column b type text;
         -- A domain constraint over a composite a view stores.
         create type at_ct1 as (a int, b int);
         create domain at_dom1 as at_ct1;
         create view at_domv1 as select '(1,2)'::at_dom1;
         alter domain at_dom1 add constraint at_dc1 check ((value).a >= 0);
         -- A composite type containing it twice.
         CREATE TYPE test_type1 AS (a int, b text);
         CREATE TYPE container_type AS (tab test_type1, arr test_type1[]);
         ALTER TYPE test_type1 ALTER ATTRIBUTE a TYPE int;
         -- Without a default, or changing persistence.
         CREATE TABLE attmp (initial int4);
         CREATE TABLE attmp2 (t text, tbl attmp);
         ALTER TABLE attmp ADD COLUMN a int4;
         ALTER TABLE attmp SET UNLOGGED;
         CREATE TABLE attmp3 (initial int4);
         CREATE VIEW attmpv AS SELECT '(1)'::attmp3 AS c;
         ALTER TABLE attmp3 ADD COLUMN b int;",
    )]);
}

// ── ATExecSetOptions: the reserved attribute option ────────────────────────

#[test]
fn the_attribute_version_option_is_ignored() {
    build_db(&[(
        "0001.sql",
        "CREATE TABLE attresv (a int);
         ALTER TABLE attresv ALTER COLUMN a SET (pg_attver = '0/1');
         ALTER TABLE attresv ALTER COLUMN a SET (pg_attver = '0/1', n_distinct = 5);
         ALTER TABLE attresv ALTER COLUMN a RESET (pg_attver, n_distinct);",
    )]);
    rejects(
        "CREATE TABLE attresv (a int);",
        "ALTER TABLE attresv ALTER COLUMN a SET (x.pg_attver = '0/1');",
        "unrecognized parameter namespace \"x\"",
    );
}

// ── Type modifiers of a function's types ───────────────────────────────────

#[test]
fn a_function_type_takes_no_modifier_its_type_cant() {
    let shell = "CREATE TYPE myvarchar;";
    for stmt in [
        "CREATE FUNCTION myvarchar_lower(text) RETURNS myvarchar(100)
           LANGUAGE internal IMMUTABLE PARALLEL SAFE STRICT AS 'lower';",
        "CREATE FUNCTION myvarchar_lower(myvarchar(100)) RETURNS text
           LANGUAGE internal IMMUTABLE PARALLEL SAFE STRICT AS 'lower';",
    ] {
        rejects(
            shell,
            stmt,
            "type modifier cannot be specified for shell type \"myvarchar\"",
        );
    }
    let defined = "CREATE TYPE myvarchar;
         CREATE FUNCTION myvarcharin(cstring, oid, integer) RETURNS myvarchar
           LANGUAGE internal IMMUTABLE PARALLEL SAFE STRICT AS 'varcharin';
         CREATE FUNCTION myvarcharout(myvarchar) RETURNS cstring
           LANGUAGE internal IMMUTABLE PARALLEL SAFE STRICT AS 'varcharout';
         CREATE TYPE myvarchar (input = myvarcharin, output = myvarcharout,
           alignment = integer, storage = main);";
    for stmt in [
        "CREATE FUNCTION myvarchar_lower(text) RETURNS myvarchar(100)
           LANGUAGE internal IMMUTABLE PARALLEL SAFE STRICT AS 'lower';",
        "CREATE FUNCTION myvarchar_lower(myvarchar(100)) RETURNS text
           LANGUAGE internal IMMUTABLE PARALLEL SAFE STRICT AS 'lower';",
    ] {
        rejects(
            defined,
            stmt,
            "type modifier is not allowed for type \"myvarchar\"",
        );
    }
}

// ── Extended statistics depend on what their expressions use ──────────────

#[test]
fn statistics_depend_on_what_their_expressions_use() {
    let setup = "CREATE TYPE pair AS (x int, y text);
         CREATE TABLE t (x int, y text);
         CREATE STATISTICS st ON (row(x, y)::pair), x FROM t;
         CREATE FUNCTION f(int) RETURNS int IMMUTABLE LANGUAGE sql AS 'select $1';
         CREATE STATISTICS st2 ON (f(x)), y FROM t;";
    rejects(
        setup,
        "DROP TYPE pair;",
        "cannot drop type pair because other objects depend on it",
    );
    rejects(
        setup,
        "DROP FUNCTION f(int);",
        "cannot drop function f(integer) because other objects depend on it",
    );
    // CASCADE drops the statistics object, whose name is free again.
    build(&[
        ("0001.sql", setup),
        (
            "0002.sql",
            "DROP FUNCTION f(int) CASCADE; CREATE STATISTICS st2 ON (x + 1), y FROM t;",
        ),
    ]);
}
