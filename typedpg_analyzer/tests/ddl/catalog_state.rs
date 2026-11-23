//! The catalog rows DDL leaves, as PostgreSQL 19 leaves them: each test
//! pins a divergence the catalog oracle (`pg_sanity`'s `catalog_state`,
//! `PG_SANITY_CATALOG=1`) found between the analyzer's catalog and the
//! server's.

use typedpg_analyzer::{ConType, PgCatalog, PgCatalogSeed, TypCategory};

fn db(sql: &str) -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(sql).unwrap();
    db
}

fn err(setup: &str, stmt: &str) -> String {
    let mut db = db(setup);
    db.apply_sql(stmt).unwrap_err().to_string()
}

fn type_row<'a>(seed: &'a PgCatalogSeed, name: &str) -> &'a typedpg_analyzer::PgType {
    seed.pg_type
        .iter()
        .find(|t| t.typname == name)
        .unwrap_or_else(|| panic!("no type {name}"))
}

fn class_oid(seed: &PgCatalogSeed, name: &str) -> typedpg_analyzer::PgClassOid {
    seed.pg_class
        .iter()
        .find(|c| c.relname == name)
        .unwrap_or_else(|| panic!("no relation {name}"))
        .oid
}

fn constraint<'a>(
    seed: &'a PgCatalogSeed,
    table: &str,
    name: &str,
) -> &'a typedpg_analyzer::PgConstraint {
    let rel = class_oid(seed, table);
    seed.pg_constraint
        .iter()
        .find(|c| c.conrelid == rel && c.conname == name)
        .unwrap_or_else(|| panic!("no constraint {name} on {table}"))
}

fn attnum(seed: &PgCatalogSeed, table: &str, column: &str) -> i16 {
    let rel = class_oid(seed, table);
    seed.pg_attribute
        .iter()
        .find(|a| a.attrelid == rel && a.attname == column)
        .unwrap_or_else(|| panic!("no column {table}.{column}"))
        .attnum
}

#[test]
fn a_domain_is_never_preferred_and_its_array_has_its_collation() {
    // DefineDomain: "domain types are never preferred"; an array of a
    // collatable domain (or base type) has the element's collation.
    let seed = db("CREATE DOMAIN d AS text COLLATE \"C\"; CREATE DOMAIN e AS text;").to_seed();
    assert!(!type_row(&seed, "d").typispreferred);
    assert_eq!(
        type_row(&seed, "_d").typcollation,
        type_row(&seed, "d").typcollation
    );
    assert!(type_row(&seed, "_e").typcollation.is_some());
}

#[test]
fn create_type_takes_its_category_preference_collatability_and_element() {
    let seed = db("CREATE TYPE t;
         CREATE FUNCTION t_in(cstring) RETURNS t LANGUAGE internal IMMUTABLE STRICT AS 'textin';
         CREATE FUNCTION t_out(t) RETURNS cstring LANGUAGE internal IMMUTABLE STRICT AS 'textout';
         CREATE TYPE t (input = t_in, output = t_out, internallength = variable,
                        category = 'S', preferred = true, collatable = true);
         CREATE TYPE p;
         CREATE FUNCTION p_in(cstring) RETURNS p LANGUAGE internal IMMUTABLE STRICT AS 'point_in';
         CREATE FUNCTION p_out(p) RETURNS cstring LANGUAGE internal IMMUTABLE STRICT AS 'point_out';
         CREATE TYPE p (input = p_in, output = p_out, internallength = 16, alignment = double,
                        element = float8);")
    .to_seed();
    let t = type_row(&seed, "t");
    assert_eq!(t.typcategory, TypCategory::String);
    assert!(t.typispreferred);
    assert!(t.typcollation.is_some());
    assert!(type_row(&seed, "_t").typcollation.is_some());
    let p = type_row(&seed, "p");
    assert!(p.typelem.is_some());
    assert!(p.typsubscript.is_some(), "raw_array_subscript_handler");
}

#[test]
fn no_row_type_has_a_cast_to_record() {
    // A row type relabels to record without a pg_cast row (can_coerce_type).
    let seed = db("CREATE TYPE c AS (a int); CREATE TABLE t (a int);").to_seed();
    let user = |oid: u32| oid >= 16384;
    assert!(seed.pg_cast.iter().all(|c| !user(c.castsource.get())));
}

#[test]
fn local_index_constraints_and_foreign_keys_are_no_inherit() {
    // index_constraint_create / addFkConstraint: NO INHERIT, but a foreign
    // key of a partitioned table, and a partition's copies.
    let seed = db("CREATE TABLE r (id int PRIMARY KEY);
         CREATE TABLE t (id int PRIMARY KEY, u int UNIQUE, f int REFERENCES r);
         CREATE TABLE p (id int, f int REFERENCES r, PRIMARY KEY (id)) PARTITION BY RANGE (id);
         CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (10);")
    .to_seed();
    assert!(constraint(&seed, "t", "t_pkey").connoinherit);
    assert!(constraint(&seed, "t", "t_u_key").connoinherit);
    assert!(constraint(&seed, "t", "t_f_fkey").connoinherit);
    assert!(constraint(&seed, "p", "p_pkey").connoinherit);
    assert!(!constraint(&seed, "p", "p_f_fkey").connoinherit);
    assert!(!constraint(&seed, "p1", "p1_pkey").connoinherit);
}

#[test]
fn a_check_key_lists_the_columns_as_read_system_ones_included() {
    // StoreRelCheck: conkey in the order the expression reads them; a
    // system column (or a field of the whole row) names the constraint.
    let seed = db("CREATE TABLE t (a int, b int, c int, CHECK (c > b),
                        CHECK (tableoid::regclass::text = 't'), CHECK ((t).a IS NOT NULL));")
    .to_seed();
    let (b, c) = (attnum(&seed, "t", "b"), attnum(&seed, "t", "c"));
    assert_eq!(constraint(&seed, "t", "t_check").conkey, vec![c, b]);
    assert_eq!(constraint(&seed, "t", "t_tableoid_check").conkey, vec![-6]);
    assert_eq!(
        constraint(&seed, "t", "t_a_check").conkey,
        vec![attnum(&seed, "t", "a")]
    );
}

#[test]
fn an_index_expression_that_is_a_column_is_the_column() {
    // ComputeIndexAttrs: "(column)", a whole-row field, a no-op cast or a
    // stripped COLLATE is the column; a WHERE that is the constant true is
    // no predicate. An exclusion constraint's expressions and predicate
    // are its index's.
    let seed = db("CREATE TABLE t (a int, b text);
         CREATE INDEX i1 ON t ((a));
         CREATE INDEX i2 ON t (((t).a));
         CREATE INDEX i3 ON t ((b::text));
         CREATE INDEX i4 ON t ((b COLLATE \"C\"));
         CREATE INDEX i5 ON t (a) WHERE true;
         CREATE TABLE e (a int, EXCLUDE ((a + 1) WITH =) WHERE (a > 0));")
    .to_seed();
    let index = |name: &str| {
        let oid = class_oid(&seed, name);
        seed.pg_index.iter().find(|i| i.indexrelid == oid).unwrap()
    };
    let (a, b) = (attnum(&seed, "t", "a"), attnum(&seed, "t", "b"));
    for (name, key) in [("i1", a), ("i2", a), ("i3", b), ("i4", b)] {
        assert_eq!(index(name).indkey, vec![key], "{name}");
        assert!(index(name).indexprs.is_empty(), "{name}");
    }
    assert_ne!(
        index("i4").indcollation,
        index("i3").indcollation,
        "COLLATE \"C\""
    );
    assert!(index("i5").indpred.is_none());
    let excl = index("e_expr_excl");
    assert_eq!((excl.indexprs.len(), excl.indpred.is_some()), (1, true));
}

#[test]
fn a_column_added_after_a_dropped_one_takes_the_next_attnum() {
    // relnatts counts the dropped columns.
    let seed = db("CREATE TABLE t (a int, b int);
         ALTER TABLE t DROP COLUMN b;
         ALTER TABLE t ADD COLUMN c int;")
    .to_seed();
    assert_eq!(attnum(&seed, "t", "c"), 3);
}

#[test]
fn an_aggregates_support_function_is_its_prosupport() {
    let seed = db(
        "CREATE FUNCTION sup(internal) RETURNS internal LANGUAGE internal
             AS 'textlike_support';
         CREATE AGGREGATE ag(int) (sfunc = int4pl, stype = int, support = sup);",
    )
    .to_seed();
    let ag = seed.pg_proc.iter().find(|p| p.proname == "ag").unwrap();
    assert!(ag.prosupport.is_some());
}

#[test]
fn a_column_alter_type_cascade_adds_to_a_typed_table_is_local() {
    // ATTypedTableRecursion passes the column definition as written.
    let seed = db("CREATE TYPE ct AS (a int);
         CREATE TABLE t OF ct;
         ALTER TYPE ct ADD ATTRIBUTE c int CASCADE;")
    .to_seed();
    let rel = class_oid(&seed, "t");
    let c = seed
        .pg_attribute
        .iter()
        .find(|a| a.attrelid == rel && a.attname == "c")
        .unwrap();
    assert!(c.attislocal);
    assert_eq!(c.attinhcount, 0);
}

#[test]
fn alter_extension_set_schema_moves_operators_and_operator_classes() {
    // AlterExtensionNamespace moves every member: an operator left behind
    // would still resolve in the old schema.
    let mut db = db("CREATE SCHEMA x; CREATE EXTENSION cube; ALTER EXTENSION cube SET SCHEMA x;");
    db.apply_sql("CREATE TABLE t (c x.cube);").unwrap();
    let seed = db.to_seed();
    let x = seed
        .pg_namespace
        .iter()
        .find(|n| n.nspname == "x")
        .unwrap()
        .oid;
    let cube_ops: Vec<_> = seed
        .pg_operator
        .iter()
        .filter(|o| o.oprname == "@>" && o.oid.get() >= 16384)
        .collect();
    assert!(!cube_ops.is_empty());
    assert!(cube_ops.iter().all(|o| o.oprnamespace == x));
    assert!(
        seed.pg_opclass
            .iter()
            .filter(|c| c.opcname == "cube_ops")
            .all(|c| c.opcnamespace == x && c.opcfamilynamespace == x)
    );
    assert_eq!(
        err(
            "CREATE SCHEMA x; CREATE EXTENSION cube; ALTER EXTENSION cube SET SCHEMA x;
             CREATE TABLE t (c x.cube);",
            "SELECT c OPERATOR(public.@>) c FROM t"
        ),
        "operator does not exist: x.cube public.@> x.cube"
    );
}

#[test]
fn a_constraint_trigger_has_a_constraint_row() {
    // CreateTriggerFiringOn: a pg_constraint row under the trigger's name,
    // which RENAME TRIGGER keeps, DROP CONSTRAINT can't drop and DROP
    // TRIGGER takes along.
    let setup = "CREATE TABLE t (a int);
         CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS 'begin return new; end';
         CREATE CONSTRAINT TRIGGER ct AFTER INSERT ON t DEFERRABLE
             FOR EACH ROW EXECUTE FUNCTION tf();";
    let seed = db(setup).to_seed();
    assert_eq!(constraint(&seed, "t", "ct").contype, ConType::Trigger);
    assert_eq!(
        err(setup, "ALTER TABLE t ADD CONSTRAINT ct CHECK (a > 0)"),
        "constraint \"ct\" for relation \"t\" already exists"
    );
    assert!(
        err(
            "CREATE TABLE t (a int, CONSTRAINT c2 CHECK (a > 0));
             CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS 'begin return new; end';",
            "CREATE CONSTRAINT TRIGGER c2 AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION tf()"
        )
        .starts_with("duplicate key value violates unique constraint")
    );
    assert!(
        err(setup, "ALTER TABLE t DROP CONSTRAINT ct")
            .starts_with("cannot drop constraint ct on table t because trigger ct on table t")
    );
    let mut renamed = db(setup);
    renamed
        .apply_sql(
            "ALTER TRIGGER ct ON t RENAME TO ct3; BEGIN; SET CONSTRAINTS ct DEFERRED; COMMIT;",
        )
        .unwrap();
    renamed.apply_sql("DROP TRIGGER ct3 ON t;").unwrap();
    let seed = renamed.to_seed();
    let t = class_oid(&seed, "t");
    assert!(seed.pg_constraint.iter().all(|c| c.conrelid != t));
}

#[test]
fn composite_types_are_extension_members_by_type_and_dropped_views_leave_none() {
    // heap_create_with_catalog records no membership for a composite
    // type's relation (its type is the member); an extension script
    // dropping a view takes its row type's membership along.
    let mut db = db("CREATE EXTENSION tablefunc;
         CREATE EXTENSION pg_stat_statements VERSION '1.4';");
    db.apply_sql("ALTER EXTENSION pg_stat_statements UPDATE;")
        .unwrap();
    let seed = db.to_seed();
    let ext_deps: Vec<_> = seed
        .pg_depend
        .iter()
        .filter(|d| d.deptype == typedpg_analyzer::DepType::Extension)
        .collect();
    let composite_rels: Vec<u32> = seed
        .pg_class
        .iter()
        .filter(|c| c.relkind == typedpg_analyzer::RelKind::CompositeType)
        .map(|c| c.oid.get())
        .collect();
    assert!(
        ext_deps
            .iter()
            .all(|d| !composite_rels.contains(&d.objid.get()))
    );
    let types: Vec<u32> = seed.pg_type.iter().map(|t| t.oid.get()).collect();
    assert!(
        ext_deps
            .iter()
            .filter(|d| d.classid.get() == 1247)
            .all(|d| types.contains(&d.objid.get()))
    );
}

#[test]
fn sequences_follow_their_tables_persistence() {
    let seed = db("CREATE UNLOGGED TABLE u (id serial);
         CREATE TABLE l (id int GENERATED ALWAYS AS IDENTITY);
         ALTER TABLE l SET UNLOGGED;
         CREATE TEMP SEQUENCE ts;
         CREATE UNLOGGED SEQUENCE us;")
    .to_seed();
    // relpersistence is not among the seed rows: the catalog oracle
    // compares it when pg_sanity mirrors this test (`pg_class.flags`).
    assert!(seed.pg_class.iter().any(|c| c.relname == "u_id_seq"));
}

#[test]
fn like_including_identity_makes_a_bigint_sequence() {
    // transformTableLikeClause's generateSerialExtraStmts names no type:
    // CREATE SEQUENCE's bigint, the source's bounds.
    let mut db = db("CREATE TABLE p (i int GENERATED ALWAYS AS IDENTITY);
         CREATE TABLE l (LIKE p INCLUDING IDENTITY);");
    // nextval of a bigint sequence past int4's bound fails only by the
    // copied MAXVALUE, not by its type.
    db.apply_sql("ALTER SEQUENCE l_i_seq MAXVALUE 3000000000;")
        .unwrap();
}

#[test]
fn a_triggers_update_of_columns_follow_a_rename() {
    let mut db = db("CREATE FUNCTION trf() RETURNS trigger LANGUAGE plpgsql
             AS 'begin return new; end';
         CREATE TABLE tt (a int, b int);
         CREATE TRIGGER t1 BEFORE UPDATE OF b ON tt FOR EACH ROW EXECUTE FUNCTION trf();
         ALTER TABLE tt RENAME COLUMN b TO b2;");
    // The trigger names column b2 now: dropping it needs CASCADE.
    let e = db.apply_sql("ALTER TABLE tt DROP COLUMN b2").unwrap_err();
    assert!(
        e.to_string()
            .starts_with("cannot drop column b2 of table tt")
    );
}

#[test]
fn a_partition_key_that_is_a_column_is_the_column() {
    // The catalog oracle compares partattrs (`pg_partitioned_table`) when
    // pg_sanity mirrors this test; a column of the key can't be dropped.
    let mut db = db("CREATE TABLE v (a int, b int) PARTITION BY RANGE (((v).a));");
    assert!(
        db.apply_sql("ALTER TABLE v DROP COLUMN a")
            .unwrap_err()
            .to_string()
            .starts_with(
                "cannot drop column \"a\" because it is part of the partition key of relation"
            )
    );
}

#[test]
fn a_partitions_index_is_created_anew_when_alter_type_rebuilds_its_parent() {
    // RememberIndexForRebuilding leaves a partition's index to its
    // parent's, whose DefineIndex names the new one from the parent
    // index's columns.
    let seed = db(
        "CREATE TABLE t1 (id int8 NOT NULL, a3 int8 NOT NULL) PARTITION BY RANGE (id);
         CREATE TABLE t1_p0 PARTITION OF t1 FOR VALUES FROM (0) TO (100);
         CREATE INDEX i11 ON t1 (a3);
         ALTER TABLE t1 RENAME COLUMN a3 TO r12;
         ALTER TABLE t1 ALTER COLUMN r12 TYPE varchar(40) USING r12::varchar(40);",
    )
    .to_seed();
    assert!(seed.pg_class.iter().any(|c| c.relname == "t1_p0_r12_idx"));
    assert!(seed.pg_class.iter().all(|c| c.relname != "t1_p0_a3_idx"));
}

#[test]
fn a_view_column_of_a_domain_type_has_no_typmod() {
    let seed = db("CREATE DOMAIN d1 AS numeric(8,2);
         CREATE TABLE t (a d1);
         CREATE VIEW v AS SELECT a, a::d1 AS b FROM t;")
    .to_seed();
    let v = class_oid(&seed, "v");
    assert!(
        seed.pg_attribute
            .iter()
            .filter(|a| a.attrelid == v)
            .all(|a| a.atttypmod.is_none())
    );
}

#[test]
fn drop_type_cascade_takes_a_dropped_columns_constraints_and_indexes() {
    let seed = db("CREATE TYPE ct AS (f1 int);
         CREATE TABLE t (id int, c ct NOT NULL, CHECK (c IS NOT NULL));
         CREATE INDEX ti ON t (c);
         DROP TYPE ct CASCADE;")
    .to_seed();
    let t = class_oid(&seed, "t");
    assert!(seed.pg_constraint.iter().all(|c| c.conrelid != t));
    assert!(seed.pg_class.iter().all(|c| c.relname != "ti"));
}

#[test]
fn errors_follow_postgres_order_and_wording() {
    // transformAlterTableStmt's sequence type check before
    // ATExecAddIdentity's NOT NULL one; a missing schema before a missing
    // object; RemoveInheritance names a partition under a partitioned
    // parent.
    assert_eq!(
        err(
            "CREATE TABLE t (a text);",
            "ALTER TABLE t ALTER COLUMN a ADD GENERATED BY DEFAULT AS IDENTITY"
        ),
        "identity column type must be smallint, integer, or bigint"
    );
    assert_eq!(
        err(
            "CREATE DOMAIN d AS int; CREATE TABLE t (a d NOT NULL);",
            "ALTER TABLE t ALTER COLUMN a ADD GENERATED BY DEFAULT AS IDENTITY"
        ),
        "identity column type must be smallint, integer, or bigint"
    );
    assert_eq!(
        err(
            "CREATE TABLE t (a int);",
            "CREATE TRIGGER g BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION nos.f()"
        ),
        "schema \"nos\" does not exist"
    );
    assert_eq!(
        err("", "CREATE TABLE c (x text) INHERITS (nos.t)"),
        "schema \"nos\" does not exist"
    );
    assert_eq!(
        err("", "ALTER DOMAIN nos.d DROP NOT NULL"),
        "schema \"nos\" does not exist"
    );
    assert_eq!(
        err("", "CREATE TABLE f (r int REFERENCES nos.t)"),
        "schema \"nos\" does not exist"
    );
    assert_eq!(
        err(
            "CREATE TABLE tt (a int); CREATE TABLE p (a int) PARTITION BY RANGE (a);",
            "ALTER TABLE tt NO INHERIT p"
        ),
        "relation \"tt\" is not a partition of relation \"p\""
    );
}

#[test]
fn alter_type_re_adds_a_not_valid_check_not_valid_down_the_tree() {
    // ATPostAlterTypeCleanup re-adds the parent's NOT VALID constraint
    // from its definition; the re-add recreates the children's copies.
    let seed = db(
        "CREATE TABLE t (id int8, CONSTRAINT k CHECK (id IS NOT NULL) NOT VALID);
         CREATE TABLE c (x text) INHERITS (t);",
    )
    .to_seed();
    assert!(constraint(&seed, "c", "k").convalidated);
    let seed = db("CREATE TABLE t (id int8);
         ALTER TABLE t ADD CONSTRAINT k CHECK (id IS NOT NULL) NOT VALID;
         CREATE TABLE c (x text) INHERITS (t);
         ALTER TABLE t ALTER COLUMN id TYPE varchar(40);")
    .to_seed();
    assert!(!constraint(&seed, "c", "k").convalidated);
}

#[test]
fn drop_type_cascade_drops_the_views_over_its_columns() {
    // A view whose column is of the type, or that reads a table column of
    // it, goes with the type (not just its column).
    let seed = db("CREATE TYPE e AS ENUM ('l0', 'l1');
         CREATE TABLE t (id int, a e, b text);
         CREATE VIEW v AS SELECT id, a, b FROM t;
         CREATE VIEW w AS SELECT id FROM t WHERE a IS NOT NULL;
         DROP TYPE e CASCADE;")
    .to_seed();
    assert!(
        seed.pg_class
            .iter()
            .all(|c| c.relname != "v" && c.relname != "w")
    );
    let t = class_oid(&seed, "t");
    assert_eq!(
        seed.pg_attribute.iter().filter(|a| a.attrelid == t).count(),
        2
    );
}

#[test]
fn pgvector_types_take_their_dimensions_as_typmod() {
    // vector_typmod_in / halfvec_typmod_in / sparsevec_typmod_in: the
    // dimension count is the typmod; their own messages (pgvector 0.8).
    let seed = db("CREATE EXTENSION vector;
         CREATE TABLE a (v vector(3), h halfvec(1024), s sparsevec(5));")
    .to_seed();
    let a = class_oid(&seed, "a");
    let typmods: Vec<Option<i32>> = seed
        .pg_attribute
        .iter()
        .filter(|x| x.attrelid == a)
        .map(|x| x.atttypmod)
        .collect();
    assert_eq!(typmods, vec![Some(3), Some(1024), Some(5)]);
    for (column, message) in [
        ("vector(0)", "dimensions for type vector must be at least 1"),
        (
            "vector(16001)",
            "dimensions for type vector cannot exceed 16000",
        ),
        ("vector(1, 2)", "invalid type modifier"),
        (
            "halfvec(16001)",
            "dimensions for type halfvec cannot exceed 16000",
        ),
        (
            "sparsevec(1000000001)",
            "dimensions for type sparsevec cannot exceed 1000000000",
        ),
    ] {
        assert_eq!(
            err(
                "CREATE EXTENSION vector;",
                &format!("CREATE TABLE b (v {column})")
            ),
            message
        );
    }
}
