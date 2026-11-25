//! The dependencies DDL records, as PostgreSQL 19 records them: each test
//! pins a divergence the catalog oracle's `pg_depend` and stored-expression
//! layers (`pg_sanity`'s `catalog_state`, `PG_SANITY_CATALOG=1`) found
//! between the analyzer's catalog and the server's — by the analyzer's
//! `pg_depend` rows where it records the dependency itself, by what a DROP
//! then does, or (for what only the oracle reads) by running the DDL under
//! `scripts/run-pg-sanity.sh`.

use typedpg_analyzer::{DepType, PgCatalog, PgCatalogSeed};

const CLASS: u32 = 1259;
const TYPE: u32 = 1247;
const PROC: u32 = 1255;
const CONSTRAINT: u32 = 2606;
const POLICY: u32 = 3256;
const REWRITE: u32 = 2618;

fn db(sql: &str) -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(sql).unwrap();
    db
}

fn err(setup: &str, stmt: &str) -> String {
    let mut db = db(setup);
    db.apply_sql(stmt).unwrap_err().to_string()
}

fn rel(seed: &PgCatalogSeed, name: &str) -> u32 {
    seed.pg_class
        .iter()
        .find(|c| c.relname == name)
        .unwrap_or_else(|| panic!("no relation {name}"))
        .oid
        .get()
}

fn typ(seed: &PgCatalogSeed, name: &str) -> u32 {
    seed.pg_type
        .iter()
        .find(|t| t.typname == name && t.oid.get() >= 16384)
        .unwrap_or_else(|| panic!("no type {name}"))
        .oid
        .get()
}

fn proc_(seed: &PgCatalogSeed, name: &str) -> u32 {
    seed.pg_proc
        .iter()
        .find(|p| p.proname == name)
        .unwrap_or_else(|| panic!("no function {name}"))
        .oid
        .get()
}

fn attnum(seed: &PgCatalogSeed, table: &str, column: &str) -> i16 {
    let rel = rel(seed, table);
    seed.pg_attribute
        .iter()
        .find(|a| a.attrelid.get() == rel && a.attname == column)
        .unwrap_or_else(|| panic!("no column {table}.{column}"))
        .attnum
}

fn constraint(seed: &PgCatalogSeed, name: &str) -> u32 {
    seed.pg_constraint
        .iter()
        .find(|c| c.conname == name)
        .unwrap_or_else(|| panic!("no constraint {name}"))
        .oid
        .get()
}

/// Whether the analyzer's `pg_depend` holds `depender -> referenced`; a
/// depender OID of 0 stands for any object of its catalog.
fn depends(
    seed: &PgCatalogSeed,
    depender: (u32, u32, i16),
    referenced: (u32, u32, i16),
    deptype: DepType,
) -> bool {
    seed.pg_depend.iter().any(|d| {
        d.classid.get() == depender.0
            && (depender.1 == 0 || d.objid.get() == depender.1)
            && d.objsubid == depender.2
            && (d.refclassid.get(), d.refobjid.get(), d.refobjsubid) == referenced
            && d.deptype == deptype
    })
}

#[test]
fn a_view_depends_on_the_columns_it_reads_not_on_their_relation() {
    // recordDependencyOnExpr → eliminate_duplicate_dependencies: a relation
    // referenced along with one of its columns is recorded as the column.
    let seed = db("CREATE TABLE t (a int, b int); CREATE VIEW v AS SELECT a FROM t;").to_seed();
    let (t, v) = (rel(&seed, "t"), rel(&seed, "v"));
    let a = attnum(&seed, "t", "a");
    assert!(depends(
        &seed,
        (CLASS, v, 0),
        (CLASS, t, a),
        DepType::Normal
    ));
    assert!(!depends(
        &seed,
        (CLASS, v, 0),
        (CLASS, t, 0),
        DepType::Normal
    ));
}

#[test]
fn a_column_default_depends_on_the_type_it_produces() {
    // StoreAttrDefault → recordDependencyOnSingleRelExpr: the default's
    // result type, the column's, a domain here.
    let seed = db("CREATE DOMAIN dm AS int; CREATE TABLE t (d dm DEFAULT NULL);").to_seed();
    let (t, dm) = (rel(&seed, "t"), typ(&seed, "dm"));
    assert!(depends(
        &seed,
        (CLASS, t, 1),
        (TYPE, dm, 0),
        DepType::Normal
    ));
    // ... recorded anew when ALTER COLUMN TYPE re-coerces it.
    let seed = db(
        "CREATE DOMAIN dplain AS int; CREATE TABLE t (c int DEFAULT 1);
         ALTER TABLE t ALTER c TYPE dplain;",
    )
    .to_seed();
    let (t, dplain) = (rel(&seed, "t"), typ(&seed, "dplain"));
    assert!(depends(
        &seed,
        (CLASS, t, 1),
        (TYPE, dplain, 0),
        DepType::Normal
    ));
}

#[test]
fn a_constant_depends_on_its_type() {
    // find_expr_references_walker: a Const records its type.
    let seed = db("CREATE TYPE kind AS ENUM ('a', 'b');
         CREATE TABLE things (k kind, CONSTRAINT things_check CHECK (k <> 'a'));")
    .to_seed();
    let (check, kind) = (constraint(&seed, "things_check"), typ(&seed, "kind"));
    assert!(depends(
        &seed,
        (CONSTRAINT, check, 0),
        (TYPE, kind, 0),
        DepType::Normal
    ));
}

#[test]
fn a_coercion_is_a_dependency_of_what_it_builds() {
    // coerce_to_target_type: an assignment cast is its function's call; an
    // argument relabeled to its parameter's type, a node of that type.
    let seed = db("CREATE TYPE pair AS (a int);
         CREATE FUNCTION to_pair(int) RETURNS pair LANGUAGE sql IMMUTABLE
           AS 'SELECT ROW($1)::pair';
         CREATE CAST (int AS pair) WITH FUNCTION to_pair(int) AS ASSIGNMENT;
         CREATE TABLE w (p pair DEFAULT 1);")
    .to_seed();
    let (w, to_pair) = (rel(&seed, "w"), proc_(&seed, "to_pair"));
    assert!(depends(
        &seed,
        (CLASS, w, 1),
        (PROC, to_pair, 0),
        DepType::Normal
    ));
    let seed = db("CREATE EXTENSION cube; CREATE DOMAIN earth AS cube;
         CREATE FUNCTION d(a earth, b earth) RETURNS float8 LANGUAGE sql
           RETURN cube_distance(a, b);")
    .to_seed();
    let (d, cube) = (proc_(&seed, "d"), typ(&seed, "cube"));
    assert!(depends(
        &seed,
        (PROC, d, 0),
        (TYPE, cube, 0),
        DepType::Normal
    ));
}

#[test]
fn a_cast_to_the_type_a_value_already_has_is_no_dependency() {
    // coerce_type builds nothing for a value of the target type.
    let seed = db("CREATE DOMAIN d1 AS numeric(8,2); CREATE TABLE t (a d1);
         CREATE VIEW v AS SELECT a::d1 AS b FROM t;")
    .to_seed();
    let (v, d1) = (rel(&seed, "v"), typ(&seed, "d1"));
    assert!(!depends(
        &seed,
        (CLASS, v, 0),
        (TYPE, d1, 0),
        DepType::Normal
    ));
}

#[test]
fn a_constraints_index_depends_on_the_columns_its_expressions_read() {
    // index_create: an exclusion constraint's index records its
    // expressions' references like any index (its own columns
    // automatically).
    let seed = db("CREATE TABLE e (a int, EXCLUDE ((a + 1) WITH =));").to_seed();
    let (e, index) = (rel(&seed, "e"), rel(&seed, "e_expr_excl"));
    let a = attnum(&seed, "e", "a");
    assert!(depends(
        &seed,
        (CLASS, index, 0),
        (CLASS, e, a),
        DepType::Auto
    ));
}

#[test]
fn a_rule_depends_on_its_relation_through_old_and_new() {
    // InsertRule: every action's range table holds OLD and NEW (DO
    // NOTHING's too).
    let seed = db("CREATE TABLE r (id int);
         CREATE RULE r_noop AS ON UPDATE TO r DO INSTEAD NOTHING;")
    .to_seed();
    let r = rel(&seed, "r");
    assert!(depends(
        &seed,
        (REWRITE, 0, 0),
        (CLASS, r, 0),
        DepType::Normal
    ));
}

#[test]
fn a_trigger_when_condition_depends_on_what_it_calls() {
    // CreateTrigger → recordDependencyOnExpr over the WHEN condition.
    let setup = "CREATE TABLE t (a int);
         CREATE FUNCTION ok(int) RETURNS bool LANGUAGE sql IMMUTABLE RETURN $1 > 0;
         CREATE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql
           AS $$ BEGIN RETURN NEW; END $$;
         CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW WHEN (ok(NEW.a))
           EXECUTE FUNCTION trg();";
    assert!(
        err(setup, "DROP FUNCTION ok(int)")
            .starts_with("cannot drop function ok(integer) because other objects depend on it"),
    );
}

#[test]
fn a_policy_depends_on_what_each_clause_reads() {
    // CreatePolicy records each clause apart: USING reads a column, WITH
    // CHECK the relation whole (a sub-select's range table).
    let seed = db("CREATE TABLE t (a int);
         CREATE POLICY p3 ON t USING (t.a > 0) WITH CHECK ((SELECT count(*) FROM t) > 0);")
    .to_seed();
    let t = rel(&seed, "t");
    let a = attnum(&seed, "t", "a");
    assert!(depends(
        &seed,
        (POLICY, 0, 0),
        (CLASS, t, 0),
        DepType::Normal
    ));
    assert!(depends(
        &seed,
        (POLICY, 0, 0),
        (CLASS, t, a),
        DepType::Normal
    ));
}

#[test]
fn a_foreign_data_wrapper_depends_on_its_functions() {
    // CreateForeignDataWrapper: on its handler, validator and connection
    // functions.
    let setup = "CREATE FUNCTION v(text[], oid) RETURNS void LANGUAGE internal
           AS 'postgresql_fdw_validator';
         CREATE FOREIGN DATA WRAPPER w VALIDATOR v;";
    assert!(
        err(setup, "DROP FUNCTION v(text[], oid)")
            .starts_with("cannot drop function v(text[],oid) because other objects depend on it"),
    );
}

#[test]
fn a_text_search_template_depends_on_its_functions() {
    // DefineTSTemplate → makeTSTemplateDependencies.
    let setup = "CREATE FUNCTION my_lexize(internal, internal, internal, internal)
           RETURNS internal LANGUAGE internal STRICT AS 'dsimple_lexize';
         CREATE TEXT SEARCH TEMPLATE tt (LEXIZE = my_lexize);";
    assert!(
        err(
            setup,
            "DROP FUNCTION my_lexize(internal, internal, internal, internal)"
        )
        .starts_with("cannot drop function my_lexize(internal,internal,internal,internal)"),
    );
}

#[test]
fn a_type_depends_on_the_support_functions_alter_type_gives_it() {
    // AlterType → GenerateTypeDependencies(rebuild): cube's RECEIVE and
    // SEND come from its 1.4 → 1.5 update script; a domain over it takes
    // its output and send functions (DefineDomain).
    let seed = db("CREATE EXTENSION cube; CREATE DOMAIN earth AS cube;").to_seed();
    let cube = typ(&seed, "cube");
    let earth = typ(&seed, "earth");
    let recv = proc_(&seed, "cube_recv");
    assert!(depends(
        &seed,
        (TYPE, cube, 0),
        (PROC, recv, 0),
        DepType::Normal
    ));
    for f in ["cube_out", "cube_send"] {
        let f = proc_(&seed, f);
        assert!(depends(
            &seed,
            (TYPE, earth, 0),
            (PROC, f, 0),
            DepType::Normal
        ));
    }
}

#[test]
fn a_routine_depends_on_the_transforms_it_uses() {
    // ProcedureCreate: a TRANSFORM FOR TYPE routine depends on the
    // transform, which DROP TRANSFORM then needs CASCADE for.
    let setup = "CREATE TRANSFORM FOR int LANGUAGE sql
           (FROM SQL WITH FUNCTION prsd_lextype(internal));
         CREATE FUNCTION f() RETURNS int LANGUAGE sql TRANSFORM FOR TYPE int AS 'select 1';";
    assert_eq!(
        err(setup, "DROP TRANSFORM FOR int LANGUAGE sql"),
        "cannot drop transform for integer language sql because other objects depend on it \
         (function f() depends on transform for integer language sql)"
    );
    let mut db = db(setup);
    db.apply_sql("DROP TRANSFORM FOR int LANGUAGE sql CASCADE")
        .unwrap();
    assert!(db.to_seed().pg_proc.iter().all(|p| p.proname != "f"));
}

#[test]
fn a_user_mapping_for_current_user_is_the_current_roles() {
    // get_rolespec_oid: CURRENT_USER is the role, by whatever name.
    let setup = "CREATE ROLE alice;
         CREATE FOREIGN DATA WRAPPER w;
         CREATE SERVER s FOREIGN DATA WRAPPER w;
         GRANT USAGE ON FOREIGN SERVER s TO alice;
         SET ROLE alice;
         CREATE USER MAPPING FOR current_user SERVER s;
         RESET ROLE;";
    assert_eq!(
        err(setup, "CREATE USER MAPPING FOR alice SERVER s"),
        "user mapping for \"alice\" already exists for server \"s\""
    );
}

#[test]
fn a_reg_constant_is_the_object_it_names() {
    // A reg* constant is its object's OID however it is written: the
    // inherited CHECK is the child's own, merged.
    db("CREATE TABLE r ();
         CREATE TABLE p (a regclass, CONSTRAINT ck CHECK (a <> 'public.r'::regclass));
         CREATE TABLE c (a regclass, CONSTRAINT ck CHECK (a <> 'r'::regclass)) INHERITS (p);");
}

#[test]
fn catalog_rows_of_statistics_sequences_publications_and_partitions() {
    // Rows the catalog oracle compares (stxkeys / stxkind / stxexprs,
    // seqcycle, the publish flags, a reattached partition's foreign-key
    // rows and their index): checked under scripts/run-pg-sanity.sh.
    db("CREATE TABLE t (a int, b int, c int);
         CREATE STATISTICS s1 ON a, b FROM t;
         CREATE STATISTICS s2 (ndistinct) ON a, (b + c) FROM t;
         CREATE STATISTICS s3 ON (a + b) FROM t;
         CREATE SEQUENCE q CYCLE;
         CREATE SEQUENCE q2;
         ALTER SEQUENCE q2 CYCLE;
         CREATE PUBLICATION pub FOR TABLE t WITH (publish = 'insert, delete');
         ALTER PUBLICATION pub SET (publish_generated_columns = stored);
         CREATE TABLE p (id int PRIMARY KEY) PARTITION BY RANGE (id);
         CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (10);
         CREATE TABLE r (pid int REFERENCES p);
         ALTER TABLE p DETACH PARTITION p1;
         ALTER TABLE p ATTACH PARTITION p1 FOR VALUES FROM (0) TO (10);");
}

#[test]
fn a_replaced_operator_estimator_is_no_longer_a_dependency() {
    // AlterOperator → makeOperatorDependencies(isUpdate): ltree's update
    // scripts move `<@` from ltreeparentsel to matchingsel.
    let mut db = db(
        "CREATE FUNCTION eq3(int, int) RETURNS bool LANGUAGE sql IMMUTABLE
           RETURN $1 = $2;
         CREATE FUNCTION mysel(internal, oid, internal, int4) RETURNS float8
           LANGUAGE internal STABLE STRICT AS 'eqsel';
         CREATE OPERATOR === (LEFTARG = int, RIGHTARG = int, FUNCTION = eq3,
           RESTRICT = mysel);
         ALTER OPERATOR === (int, int) SET (RESTRICT = eqsel);",
    );
    db.apply_sql("DROP FUNCTION mysel(internal, oid, internal, int4)")
        .unwrap();
}

#[test]
fn a_domain_over_a_domain_takes_a_copy_of_its_default() {
    // DefineDomain copies the base domain's typdefaultbin (`pg_type`
    // DEFAULT, compared under scripts/run-pg-sanity.sh), which dropping
    // the base's default later leaves.
    db("CREATE DOMAIN d1 AS int DEFAULT 5; CREATE DOMAIN d2 AS d1;
         ALTER DOMAIN d1 DROP DEFAULT;
         CREATE TABLE t (id int, x d2);");
}

#[test]
fn a_partitions_copied_default_depends_on_what_the_parents_does() {
    // The default a partition takes from its parent is the same
    // expression: it depends on the same objects.
    let seed = db("CREATE DOMAIN nn_text AS text NOT NULL;
         CREATE FUNCTION g(int) RETURNS int LANGUAGE sql IMMUTABLE RETURN $1;
         CREATE TABLE p (a int, d nn_text DEFAULT 'd', e int DEFAULT g(1))
           PARTITION BY LIST (a);
         CREATE TABLE p1 PARTITION OF p FOR VALUES IN (1);")
    .to_seed();
    let p1 = rel(&seed, "p1");
    let (nn_text, g) = (typ(&seed, "nn_text"), proc_(&seed, "g"));
    let (d, e) = (attnum(&seed, "p1", "d"), attnum(&seed, "p1", "e"));
    assert!(depends(
        &seed,
        (CLASS, p1, d),
        (TYPE, nn_text, 0),
        DepType::Normal
    ));
    assert!(depends(
        &seed,
        (CLASS, p1, e),
        (PROC, g, 0),
        DepType::Normal
    ));
}

#[test]
fn an_array_type_depends_on_its_elements_typmod_functions() {
    // DefineType: the array type takes the element's typmodin / typmodout.
    let seed = db("CREATE EXTENSION vector;").to_seed();
    let (array, typmod_in) = (typ(&seed, "_vector"), proc_(&seed, "vector_typmod_in"));
    assert!(depends(
        &seed,
        (TYPE, array, 0),
        (PROC, typmod_in, 0),
        DepType::Normal
    ));
}

#[test]
fn catalog_rows_of_rules_families_and_renames() {
    // What only the oracle reads, under scripts/run-pg-sanity.sh: a rule's
    // dependency on its relation through `NEW.*` and `*` over another FROM
    // item, a FOR ORDER BY member of an operator family, a partition key
    // expression and an enum label in an array literal after renames.
    db(r#"CREATE TABLE tt (a int, b text);
         CREATE TABLE log (a int, b text);
         CREATE RULE r1 AS ON UPDATE TO tt DO ALSO INSERT INTO log VALUES (NEW.*);
         CREATE VIEW lv AS SELECT a FROM log;
         CREATE RULE r2 AS ON INSERT TO tt DO ALSO SELECT * FROM lv;
         CREATE EXTENSION cube;
         CREATE TABLE p (a int, b int) PARTITION BY RANGE ((a + 0));
         ALTER TABLE p RENAME COLUMN a TO x;
         CREATE TYPE kind AS ENUM ('a', 'b');
         CREATE TABLE w (k kind, CHECK (k <> ALL ('{a, "b"}')));
         ALTER TYPE kind RENAME VALUE 'a' TO 'z';"#);
}

#[test]
fn a_return_rule_written_as_a_rule_depends_on_its_view() {
    // transformRuleStmt: a CREATE RULE action's range table holds OLD and
    // NEW — the view itself, which the `_RETURN` rule then depends on
    // (kept on its `pg_rewrite` row); the view still drops.
    let setup = "CREATE TABLE t (a int); CREATE TABLE u (a int);
         CREATE VIEW v AS SELECT a FROM t;
         CREATE OR REPLACE RULE \"_RETURN\" AS ON SELECT TO v DO INSTEAD SELECT a FROM u;";
    let mut db = db(setup);
    let seed = db.to_seed();
    let v = rel(&seed, "v");
    assert!(depends(
        &seed,
        (REWRITE, 0, 0),
        (CLASS, v, 0),
        DepType::Normal
    ));
    db.apply_sql("DROP VIEW v").unwrap();
}

#[test]
fn a_check_copied_by_like_depends_on_what_it_calls() {
    // expandTableLikeClause → StoreRelCheck records the copy's own
    // dependencies: with the original gone, the copy still holds the
    // function.
    let setup = "CREATE FUNCTION pos(int) RETURNS bool LANGUAGE sql IMMUTABLE AS 'SELECT $1 > 0';
         CREATE TABLE t (a int CONSTRAINT t_a_check CHECK (pos(a)));
         CREATE TABLE l (LIKE t INCLUDING ALL);
         ALTER TABLE t DROP CONSTRAINT t_a_check;";
    let message = err(setup, "DROP FUNCTION pos(int)");
    assert!(
        message.starts_with(
            "cannot drop function pos(integer) because other objects depend on it \
             (constraint t_a_check on table l depends on function pos(integer))"
        ),
        "{message}"
    );
}

#[test]
fn a_regclass_constant_follows_its_relation_renamed_and_moved() {
    // PG keeps a `nextval('s.sq')` default's sequence by OID: renamed, in
    // a renamed schema or moved, it is still the one the default calls —
    // when ALTER COLUMN TYPE rebuilds the default too.
    for rename in [
        "ALTER SEQUENCE s.sq RENAME TO sq2; ALTER SCHEMA s RENAME TO s3;",
        "CREATE SCHEMA s3; ALTER SEQUENCE s.sq SET SCHEMA s3; ALTER SEQUENCE s3.sq RENAME TO sq2;",
    ] {
        let setup = format!(
            "CREATE SCHEMA s; CREATE SEQUENCE s.sq;
             CREATE TABLE t (b int DEFAULT nextval('s.sq'));
             {rename}
             ALTER TABLE t ALTER b TYPE bigint;"
        );
        let message = err(&setup, "DROP SEQUENCE s3.sq2");
        assert!(
            message.starts_with(
                "cannot drop sequence s3.sq2 because other objects depend on it (default value \
                 for column b of table t depends on sequence s3.sq2)"
            ),
            "{rename}: {message}"
        );
    }
}

#[test]
fn stored_expressions_as_their_deparse_parses_back() {
    // What only the oracle reads: PG prints BETWEEN's AND parenthesized
    // (it parses back merged into the AND around it), and an IN list over
    // a varchar column as an array cast to text[] (it parses back as the
    // elements cast).
    db("CREATE TABLE t (m text CHECK (m IS NULL
             OR (char_length(m) BETWEEN 1 AND 72 AND m !~ '[[:cntrl:]]')),
         c varchar(10) CHECK (c IN ('x', 'y')));");
}

#[test]
fn a_row_filter_reads_a_virtual_columns_expression() {
    // publication_add_relation stores the row filter with its virtual
    // generated columns expanded: it depends on `id`, not on `gv`, which
    // then drops.
    let mut db = db(
        "CREATE TABLE t (id int, gv int8 GENERATED ALWAYS AS (id * 2) VIRTUAL);
         CREATE PUBLICATION p FOR TABLE t WHERE (gv IS NOT NULL);",
    );
    db.apply_sql("ALTER TABLE t DROP COLUMN gv;").unwrap();
}

#[test]
fn like_copies_and_renames_as_the_catalog_keeps_them() {
    // expandTableLikeClause: a copied CHECK is valid (NOT VALID or not),
    // and a temporal UNIQUE's index, copied with its exclusion operators,
    // is named `_excl`. A statistics object's expression reads a renamed
    // column by its new name (what only the oracle reads).
    let mut db = db("CREATE EXTENSION btree_gist;
         CREATE TABLE t (id int, valid_at tstzrange, UNIQUE (id, valid_at WITHOUT OVERLAPS));
         ALTER TABLE t ADD CONSTRAINT k CHECK (id IS NOT NULL) NOT VALID;
         CREATE TABLE lk (LIKE t INCLUDING ALL);
         CREATE TABLE st (a int, b int);
         CREATE STATISTICS sx ON (a IS NULL), b FROM st;
         ALTER TABLE st RENAME a TO r;");
    let seed = db.to_seed();
    let lk = rel(&seed, "lk");
    let con = |name: &str| {
        seed.pg_constraint
            .iter()
            .find(|c| c.conname == name && c.conrelid.get() == lk)
            .unwrap_or_else(|| panic!("no constraint {name}"))
    };
    assert!(con("k").convalidated);
    con("lk_id_valid_at_excl");
    db.apply_sql("ALTER TABLE st RENAME r TO a;").unwrap();
}
