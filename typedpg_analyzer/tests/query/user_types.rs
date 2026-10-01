//! Querying against user-defined types: enums, domains, composite types,
//! ranges, arrays as column types.
//!
//! Also: `alias.*` used as a composite value (e.g. fed to `row_to_json`),
//! which relies on the table's implicit composite type.

use crate::common::*;

fn setup() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE users (
            id   BIGINT PRIMARY KEY,
            name TEXT NOT NULL
        );",
    )
    .unwrap();
    db
}

fn setup_user_types() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TYPE user_role AS ENUM ('admin', 'editor', 'viewer');
         CREATE DOMAIN user_prefs AS JSONB;
         CREATE SCHEMA whatsapp;
         CREATE DOMAIN whatsapp.health_data AS JSONB;
         CREATE TABLE users (
            id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            name        TEXT NOT NULL,
            email       TEXT NOT NULL UNIQUE,
            age         INT,
            role        user_role NOT NULL DEFAULT 'viewer',
            preferences user_prefs,
            created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
         );
         CREATE TABLE whatsapp.channels (
            channel_id BIGINT PRIMARY KEY,
            health     whatsapp.health_data,
            updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
         );",
    )
    .unwrap();
    db
}

// ── `alias.*` resolves to the table's composite type ─────────────────────────

#[test]
fn star_expr_resolves_to_composite_type_via_row_to_json() {
    let db = setup();
    // `u.*` feeds into row_to_json, which takes `record` / any composite.
    let sql = "SELECT row_to_json(u.*) AS payload FROM users u";
    let info = db.analyze(sql).unwrap();
    assert_eq!(col(&info, "payload").pg_type, json_ty());
}

#[test]
fn star_expr_not_null_because_row_is_always_present() {
    let db = setup();
    let sql = "SELECT row_to_json(u.*) AS payload FROM users u";
    let info = db.analyze(sql).unwrap();
    // `alias.*` is a composite value that exists iff the row exists — and a
    // row is always present for every returned tuple → NOT NULL.
    assert!(!col(&info, "payload").nullable);
}

#[test]
fn star_expr_on_cte_resolves_to_anonymous_record() {
    // CTE rows don't have a registered composite OID — the analyzer
    // surfaces them as `pg_catalog.record` with the CTE's columns as the
    // inline record shape, mirroring how PG composes an anonymous row
    // type at planning time. `row_to_json` accepts the record and lands
    // its result on `json`.
    let db = setup();
    let s = db
        .analyze(
            "WITH u AS (SELECT id, name FROM users) \
             SELECT row_to_json(u.*) AS payload FROM u",
        )
        .unwrap();
    assert_eq!(col(&s, "payload").pg_type, json_ty());
    assert!(!col(&s, "payload").nullable);
}

#[test]
fn star_expr_on_unknown_alias_fails() {
    let db = setup();
    let sql = "SELECT row_to_json(nope.*) FROM users u";
    assert_analyze_err!(
        db.analyze(sql),
        AnalyzeError::UndefinedTable(_),
        concat!(
            "missing FROM-clause entry for table \"nope\"\n",
            "  ╭────\n",
            "1 │ SELECT row_to_json(nope.*) FROM users u\n",
            "  ·                    ────\n",
            "  ╰────\n",
        ),
    );
}

// ── Enum types (CREATE TYPE ... AS ENUM) ─────────────────────────────────────

#[test]
fn enum_column_select() {
    let db = setup_user_types();
    let s = db.analyze("SELECT id, name, role FROM users").unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int8()),
            c("name", text()),
            c(
                "role",
                enum_ty("public", "user_role", &["admin", "editor", "viewer"]),
            ),
        ],
    );
}

#[test]
fn enum_in_where() {
    let db = setup_user_types();
    let s = db.analyze("SELECT id FROM users WHERE role = $p1").unwrap();
    // $p1 inferred as the enum type.
    assert_params(
        &s,
        vec![p(enum_ty(
            "public",
            "user_role",
            &["admin", "editor", "viewer"],
        ))],
    );
}

// ── Polymorphic operator resolution with UNKNOWN literals ────────────────────
//
// PostgreSQL has no `myenum = unknown` operator — equality on user-defined
// enums goes through the polymorphic `anyenum = anyenum`. Same story for
// arrays (`anyarray = anyarray`) and ranges (`anyrange = anyrange`). When
// one side is a string literal (`unknown`), the analyzer must still resolve
// the polymorphic operator and let the unknown side be coerced to the
// bound concrete type — matching `enforce_generic_type_consistency` in PG.

#[test]
fn enum_eq_unknown_literal_resolves_via_anyenum() {
    let db = setup_user_types();
    let s = db
        .analyze("SELECT id FROM users WHERE role = 'admin'")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn enum_eq_unknown_literal_lhs_resolves_via_anyenum() {
    // Symmetric case: literal on the left, enum column on the right. The
    // resolver must bind the polymorphic type from the *non*-UNKNOWN side
    // regardless of operand order.
    let db = setup_user_types();
    let s = db
        .analyze("SELECT id FROM users WHERE 'admin' = role")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn enum_neq_unknown_literal_resolves_via_anyenum() {
    let db = setup_user_types();
    let s = db
        .analyze("SELECT id FROM users WHERE role <> 'viewer'")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn enum_in_list_of_unknown_literals_resolves_via_anyenum() {
    // `IN (...)` desugars to a chain of `=` comparisons; each unknown
    // literal must resolve against the enum on the LHS.
    let db = setup_user_types();
    let s = db
        .analyze("SELECT id FROM users WHERE role IN ('admin', 'editor')")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn enum_array_contains_unknown_literal_resolves() {
    // `arr @> '{admin}'::user_role[]` — operator is `anyarray @> anyarray`.
    // Without the cast the literal is `unknown`, and the resolver must bind
    // `anyarray` from the column-side array type.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TYPE user_role AS ENUM ('admin', 'editor', 'viewer');
         CREATE TABLE users (
            id    BIGINT PRIMARY KEY,
            roles user_role[] NOT NULL
         );",
    )
    .unwrap();
    let s = db
        .analyze("SELECT id FROM users WHERE roles @> '{admin}'")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn text_array_contains_unknown_literal_resolves() {
    // Same shape with a built-in text[] — exercises the polymorphic path
    // when the left side is a concrete array of a built-in element type.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (id INT PRIMARY KEY, tags TEXT[] NOT NULL);")
        .unwrap();
    let s = db
        .analyze("SELECT id FROM t WHERE tags @> '{a,b}'")
        .unwrap();
    assert_cols(&s, vec![c("id", int4())]);
}

#[test]
fn text_concat_with_unknown_literal_stays_text() {
    // Regression guard: `text || 'foo'` must NOT be hijacked by the
    // polymorphic `anycompatible || anycompatiblearray` (which would
    // produce `text[]`). Step 3's "exactly match the known side" filter
    // must keep `text || text` over the polymorphic candidate.
    let db = setup_user_types();
    let s = db
        .analyze("SELECT name || '!' AS greeting FROM users")
        .unwrap();
    assert_cols(&s, vec![c("greeting", text())]);
}

#[test]
fn enum_in_insert() {
    let db = setup_user_types();
    let s = db
        .analyze("INSERT INTO users (name, email, role) VALUES ($p1, $p2, $p3) RETURNING id, role")
        .unwrap();
    assert_params(
        &s,
        vec![
            p(text()),
            p(text()),
            p(enum_ty(
                "public",
                "user_role",
                &["admin", "editor", "viewer"],
            )),
        ],
    );
}

#[test]
fn enum_in_update() {
    let db = setup_user_types();
    let s = db
        .analyze("UPDATE users SET role = $p1 WHERE id = $p2 RETURNING role")
        .unwrap();
    assert_params(
        &s,
        vec![
            p(enum_ty(
                "public",
                "user_role",
                &["admin", "editor", "viewer"],
            )),
            p(int8()),
        ],
    );
}

// ── Domain types (CREATE DOMAIN) ─────────────────────────────────────────────

fn setup_scalar_domains() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE DOMAIN email AS TEXT;
         CREATE DOMAIN pos AS INT;
         CREATE TABLE t (addr email, n pos);",
    )
    .unwrap();
    db
}

#[test]
fn aggregate_over_text_domain_resolves_to_base() {
    // PG smashes a domain to its base type for function resolution, so
    // `max(email)` resolves to `max(text)` and returns text. The analyzer used
    // to reject it with "function max(email) does not exist".
    let db = setup_scalar_domains();
    let s = db.analyze("SELECT max(addr) AS m FROM t").unwrap();
    assert_cols(&s, vec![cn("m", text())]);
}

#[test]
fn aggregate_over_int_domain_picks_exact_base_overload() {
    // The base (int4) must win as an *exact* match: `max(pos)` → `max(int4)`
    // (int4, not some implicit-cast candidate like int8), and `sum(pos)` →
    // `sum(int4)` → bigint, exactly as for a plain int column.
    let db = setup_scalar_domains();
    let s = db
        .analyze("SELECT max(n) AS mx, sum(n) AS sm FROM t")
        .unwrap();
    assert_cols(&s, vec![cn("mx", int4()), cn("sm", int8())]);
}

#[test]
fn scalar_function_over_text_domain_resolves_to_base() {
    let db = setup_scalar_domains();
    let s = db
        .analyze("SELECT upper(addr) AS u, length(addr) AS l FROM t")
        .unwrap();
    assert_cols(&s, vec![cn("u", text()), cn("l", int4())]);
}

#[test]
fn param_compared_against_domain_column_infers_base_type() {
    // Operators resolve against a domain's base type, so PG's Describe reports
    // a parameter compared against a domain column as the base (`text`), not
    // the domain. The analyzer used to pin it to the domain.
    let db = setup_scalar_domains();
    let s = db.analyze("SELECT n FROM t WHERE addr <= $p0").unwrap();
    assert_params(&s, vec![p(text())]);
}

#[test]
fn domain_column_surfaces_as_domain_type() {
    // Analyzer surfaces the Domain wrapper with its base type preserved; the
    // macro crate decides whether to treat it as opaque JSONB or unwrap.
    let db = setup_user_types();
    let s = db.analyze("SELECT id, preferences FROM users").unwrap();
    assert_cols(
        &s,
        vec![
            c("id", int8()),
            cn("preferences", domain("public", "user_prefs", jsonb())),
        ],
    );
}

#[test]
fn domain_param_insert_surfaces_as_domain_type() {
    let db = setup_user_types();
    let s = db
        .analyze("INSERT INTO users (name, email, preferences) VALUES ($p1, $p2, $p3) RETURNING id")
        .unwrap();
    assert_params(
        &s,
        vec![
            p(text()),
            p(text()),
            pn(domain("public", "user_prefs", jsonb())),
        ],
    );
    // `Type::cast_name` unwraps the domain to its schema-qualified base name.
    assert_eq!(
        s.params[2].pg_type.cast_name().as_deref(),
        Some("pg_catalog.jsonb"),
    );
}

#[test]
fn domain_in_where() {
    let db = setup_user_types();
    let s = db
        .analyze("SELECT id FROM users WHERE preferences IS NOT NULL")
        .unwrap();
    assert_cols(&s, vec![c("id", int8())]);
}

#[test]
fn schema_qualified_domain_column() {
    let db = setup_user_types();
    let s = db
        .analyze("SELECT channel_id, health FROM whatsapp.channels")
        .unwrap();
    assert_cols(
        &s,
        vec![
            c("channel_id", int8()),
            cn("health", domain("whatsapp", "health_data", jsonb())),
        ],
    );
}

// ── Array column types ────────────────────────────────────────────────────

#[test]
fn text_array_column_type_resolves_to_array_kind() {
    // TEXT[] must land as an Array TypeKind in the snapshot (not OID 0).
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE TABLE t (id INT NOT NULL, tags TEXT[] NOT NULL);")
        .unwrap();

    let table = db.resolve_table(None, "t").unwrap();
    let attrs = db.attributes_of(table.oid);
    let tags_col = attrs.iter().find(|c| c.attname == "tags").unwrap();
    assert_ne!(tags_col.atttypid.get(), 0);

    let type_entry = db.get_type(tags_col.atttypid).unwrap();
    assert_eq!(
        type_entry.typcategory,
        TypCategory::Array,
        "TEXT[] should be an Array type, got {:?}",
        type_entry.typcategory
    );
}

// ── Type alias resolution ─────────────────────────────────────────────────

#[test]
fn builtin_type_aliases_resolve_to_canonical_oid() {
    // PG accepts "integer"/"int"/"bigint"/"smallint"/"boolean"/"real" as
    // aliases for int4/int4/int8/int2/bool/float4. A column declared with
    // each alias must land on the canonical OID.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE t (
            a integer NOT NULL,
            b int NOT NULL,
            c bigint NOT NULL,
            d smallint NOT NULL,
            e boolean NOT NULL,
            f real NOT NULL,
            g text NOT NULL
        );",
    )
    .unwrap();

    let table = db.resolve_table(None, "t").unwrap();
    let int4_oid = db
        .resolve_type_by_name(Some("pg_catalog"), "int4")
        .unwrap()
        .oid;
    let int8_oid = db
        .resolve_type_by_name(Some("pg_catalog"), "int8")
        .unwrap()
        .oid;
    let int2_oid = db
        .resolve_type_by_name(Some("pg_catalog"), "int2")
        .unwrap()
        .oid;
    let bool_oid = db
        .resolve_type_by_name(Some("pg_catalog"), "bool")
        .unwrap()
        .oid;
    let float4_oid = db
        .resolve_type_by_name(Some("pg_catalog"), "float4")
        .unwrap()
        .oid;
    let text_oid = db
        .resolve_type_by_name(Some("pg_catalog"), "text")
        .unwrap()
        .oid;

    let attrs = db.attributes_of(table.oid);
    assert_eq!(attrs[0].atttypid, int4_oid, "integer -> int4");
    assert_eq!(attrs[1].atttypid, int4_oid, "int -> int4");
    assert_eq!(attrs[2].atttypid, int8_oid, "bigint -> int8");
    assert_eq!(attrs[3].atttypid, int2_oid, "smallint -> int2");
    assert_eq!(attrs[4].atttypid, bool_oid, "boolean -> bool");
    assert_eq!(attrs[5].atttypid, float4_oid, "real -> float4");
    assert_eq!(attrs[6].atttypid, text_oid, "text -> text");
}

// ── Generated columns (GENERATED ALWAYS AS (expr) STORED) ──────────────────

fn setup_generated() -> PgCatalog {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE TABLE invoices (
            id        BIGINT PRIMARY KEY,
            net       NUMERIC(12,2) NOT NULL,
            tax_rate  NUMERIC(4,3)  NOT NULL,
            gross     NUMERIC(12,2) GENERATED ALWAYS AS (net * (1 + tax_rate)) STORED
         );",
    )
    .unwrap();
    db
}

#[test]
fn generated_column_select_uses_declared_type() {
    let db = setup_generated();
    // The declared type wins over the expression type.
    let s = db.analyze("SELECT gross FROM invoices").unwrap();
    assert_cols(&s, vec![cn("gross", numeric_ps(12, 2))]);
}

#[test]
fn insert_into_generated_column_rejected() {
    let db = setup_generated();
    // PG: `cannot insert a non-DEFAULT value into column "gross"`. The
    // analyzer should reject this statically — the column is generated,
    // and accepting a $p1 value here would mask a real bug.
    assert_analyze_err!(
        db.analyze(
            "INSERT INTO invoices (id, net, tax_rate, gross) \
             VALUES ($p1, $p2, $p3, $p4)",
        ),
        AnalyzeError::GeneratedAlways(_),
        "cannot insert a non-DEFAULT value into column \"gross\" (Column \"gross\" is a generated column.)",
    );
}

#[test]
fn update_generated_column_rejected() {
    let db = setup_generated();
    // PG: `column "gross" can only be updated to DEFAULT`.
    assert_analyze_err!(
        db.analyze("UPDATE invoices SET gross = $p1 WHERE id = $p2"),
        AnalyzeError::GeneratedAlways(_),
        "column \"gross\" can only be updated to DEFAULT (Column \"gross\" is a generated column.)",
    );
}

#[test]
fn insert_into_generated_column_with_literal_rejected() {
    let db = setup_generated();
    // Even a NUMERIC literal cannot be assigned to a generated column.
    assert_analyze_err!(
        db.analyze(
            "INSERT INTO invoices (id, net, tax_rate, gross) \
             VALUES ($p1, $p2, $p3, 42.0)",
        ),
        AnalyzeError::GeneratedAlways(_),
        "cannot insert a non-DEFAULT value into column \"gross\" (Column \"gross\" is a generated column.)",
    );
}

#[test]
fn update_generated_column_to_default_accepted() {
    let db = setup_generated();
    // PG accepts `UPDATE … SET gen_col = DEFAULT` (resets the computed value).
    let s = db
        .analyze("UPDATE invoices SET gross = DEFAULT WHERE id = $p1 RETURNING gross")
        .unwrap();
    assert_cols(&s, vec![cn("gross", numeric_ps(12, 2))]);
}

#[test]
fn insert_into_generated_column_with_default_keyword_accepted() {
    let db = setup_generated();
    // `DEFAULT` is the only value PG accepts for a generated column. The
    // analyzer must not reject this case.
    let s = db
        .analyze(
            "INSERT INTO invoices (id, net, tax_rate, gross) \
             VALUES ($p1, $p2, $p3, DEFAULT) RETURNING gross",
        )
        .unwrap();
    assert_cols(&s, vec![cn("gross", numeric_ps(12, 2))]);
}

#[test]
fn insert_skipping_generated_column_accepted() {
    let db = setup_generated();
    // The standard pattern: just leave the generated column out of the
    // column list. PG fills it in itself.
    let s = db
        .analyze(
            "INSERT INTO invoices (id, net, tax_rate) \
             VALUES ($p1, $p2, $p3) RETURNING gross",
        )
        .unwrap();
    assert_cols(&s, vec![cn("gross", numeric_ps(12, 2))]);
}

// ── Domain with NOT NULL — `pg_type.typnotnull` propagates to columns ──────
//
// `CREATE DOMAIN d AS T NOT NULL` makes every column declared as `d`
// non-nullable in PG, even when the column itself omits `NOT NULL`. The
// constraint also fires on direct INSERT/UPDATE of literal `NULL`. The
// catalog mirror carries `pg_type.typnotnull`; the analyzer walks the
// `typbasetype` chain so a domain-of-a-domain inherits the constraint.

#[test]
fn domain_not_null_propagates_to_column_nullability() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE DOMAIN nn_int AS INT NOT NULL;
         CREATE TABLE t (id BIGINT PRIMARY KEY, x nn_int);",
    )
    .unwrap();
    // PG: `x` is NOT NULL (domain forbids nulls), regardless of column-level
    // declaration.
    let s = db.analyze("SELECT x FROM t").unwrap();
    assert_cols(&s, vec![c("x", domain("public", "nn_int", int4()))]);
}

#[test]
fn insert_null_into_nn_domain_column_is_rejected() {
    // PG only catches the domain-not-null violation at runtime; the
    // analyzer catches it at compile time. PG sanity's `prepare` doesn't
    // reach runtime, so opt out of the mirror.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE DOMAIN nn_int AS INT NOT NULL;
         CREATE TABLE t (id BIGINT PRIMARY KEY, x nn_int);",
    )
    .unwrap();
    assert_analyze_err!(
        db.analyze("INSERT INTO t (id, x) VALUES ($p1, NULL)"),
        AnalyzeError::Invalid(_),
        "domain nn_int does not allow null values",
    );
}

#[test]
fn update_null_into_nn_domain_column_is_rejected() {
    // The pg_sanity execute fallback runs UPDATE with NULL params on a
    // freshly-created scratch table — zero rows match WHERE so the
    // domain-not-null check never fires at runtime. Keep the skip and
    // rely on the analyzer's compile-time guard.
    let mut db = PgCatalog::new().unwrap();
    db.skip_pg_sanity();
    db.apply_sql(
        "CREATE DOMAIN nn_int AS INT NOT NULL;
         CREATE TABLE t (id BIGINT PRIMARY KEY, x nn_int);",
    )
    .unwrap();
    assert_analyze_err!(
        db.analyze("UPDATE t SET x = NULL WHERE id = $p1"),
        AnalyzeError::Invalid(_),
        "domain nn_int does not allow null values",
    );
}

#[test]
fn nn_domain_chain_propagates_through_intermediate_domain() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE DOMAIN base_int AS INT;
         CREATE DOMAIN nn_int AS base_int NOT NULL;
         CREATE TABLE t (id BIGINT PRIMARY KEY, x nn_int);",
    )
    .unwrap();
    // Even though `base_int` allows NULL and the column has no explicit
    // NOT NULL, walking the `typbasetype` chain finds `nn_int` and the
    // analyzer must treat `x` as not nullable.
    let s = db.analyze("SELECT x FROM t").unwrap();
    assert_cols(
        &s,
        vec![c(
            "x",
            domain("public", "nn_int", domain("public", "base_int", int4())),
        )],
    );
}

#[test]
fn nullable_domain_does_not_force_non_null() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE DOMAIN maybe_int AS INT;
         CREATE TABLE t (id BIGINT PRIMARY KEY, x maybe_int);",
    )
    .unwrap();
    // Sanity check: a plain (nullable) domain should not promote the column
    // to NOT NULL — `x` stays nullable.
    let s = db.analyze("SELECT x FROM t").unwrap();
    assert_cols(&s, vec![cn("x", domain("public", "maybe_int", int4()))]);
    // And inserting NULL is allowed.
    db.analyze("INSERT INTO t (id, x) VALUES ($p1, NULL)")
        .unwrap();
}

#[test]
fn returning_nn_domain_column_is_not_nullable() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE DOMAIN nn_int AS INT NOT NULL;
         CREATE TABLE t (id BIGINT PRIMARY KEY, x nn_int);",
    )
    .unwrap();
    let s = db
        .analyze("INSERT INTO t (id, x) VALUES ($p1, $p2) RETURNING x")
        .unwrap();
    assert_cols(&s, vec![c("x", domain("public", "nn_int", int4()))]);
}

#[test]
fn schema_qualified_domain_param() {
    let db = setup_user_types();
    let s = db
        .analyze(
            "INSERT INTO whatsapp.channels (channel_id, health, updated_at) \
             VALUES ($p1, $p2, now())",
        )
        .unwrap();
    assert_params(
        &s,
        vec![p(int8()), pn(domain("whatsapp", "health_data", jsonb()))],
    );
    assert_eq!(
        s.params[1].pg_type.cast_name().as_deref(),
        Some("pg_catalog.jsonb"),
    );
}

// ── Domains in common-type resolution (select_common_type) ──────────────────

#[test]
fn all_same_domain_branches_preserve_domain() {
    // PG's first pass keeps the type when *every* input is identical —
    // `COALESCE(d, d)` stays the domain.
    let db = setup_scalar_domains();
    let s = db
        .analyze("SELECT COALESCE(addr, addr) AS v FROM t")
        .unwrap();
    assert_cols(&s, vec![cn("v", domain("public", "email", text()))]);
}

#[test]
fn domain_with_null_branch_resolves_to_base() {
    // A NULL alongside sends the resolution through PG's main loop, which
    // smashes every input to its base type — `COALESCE(d, NULL)` is text.
    let db = setup_scalar_domains();
    let s = db
        .analyze("SELECT COALESCE(addr, NULL) AS v FROM t")
        .unwrap();
    assert_cols(&s, vec![cn("v", text())]);

    let s = db.analyze("SELECT GREATEST(n, NULL) AS v FROM t").unwrap();
    assert_cols(&s, vec![cn("v", int4())]);
}

#[test]
fn case_without_else_smashes_domain_to_base() {
    // The implicit `ELSE NULL` participates in common-type resolution, so a
    // missing ELSE also degrades the domain to its base type.
    let db = setup_scalar_domains();
    let s = db
        .analyze("SELECT CASE WHEN true THEN addr END AS v FROM t")
        .unwrap();
    assert_cols(&s, vec![cn("v", text())]);
    // With an explicit same-domain ELSE the domain is preserved.
    let s = db
        .analyze("SELECT CASE WHEN true THEN addr ELSE addr END AS v FROM t")
        .unwrap();
    assert_cols(&s, vec![cn("v", domain("public", "email", text()))]);
}

#[test]
fn cross_domain_same_base_coerces_like_pg() {
    // Two distinct domains over the same base are mutually coercible — PG
    // reduces the source domain to its base and wraps the target domain's
    // checks around it (verified on PG 18 with a function taking one domain
    // called with the other). Both assignment and expression contexts.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE DOMAIN email AS TEXT;
         CREATE DOMAIN handle AS TEXT;
         CREATE TABLE t2 (a email, b handle);",
    )
    .unwrap();
    db.analyze("INSERT INTO t2 (a) SELECT b FROM t2").unwrap();
    db.analyze("UPDATE t2 SET a = b").unwrap();
    let s = db.analyze("SELECT a = b AS eq FROM t2").unwrap();
    assert_cols(&s, vec![cn("eq", bool_ty())]);
}

/// An array of a domain is not itself a domain: PG's row description keeps
/// it as the domain's array type (only a domain column collapses to its
/// base), while `ARRAY[domain_value]` is built over the base type.
#[test]
fn arrays_of_domains_keep_the_domain_array_type() {
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql(
        "CREATE DOMAIN nn2 AS int NOT NULL;
         CREATE TABLE dt (d nn2, ds nn2[]);",
    )
    .unwrap();
    for sql in [
        "SELECT '{1}'::nn2[] AS v",
        "SELECT ds AS v FROM dt",
        "SELECT ARRAY[d] AS v FROM dt",
        "SELECT d AS v FROM dt",
    ] {
        db.analyze(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

#[test]
fn an_array_of_a_domain_is_typed_as_the_domains_array() {
    // The analyzer's type for an array of a domain is the domain's own
    // array type (`d[]`), the one Describe reports.
    let mut db = PgCatalog::new().unwrap();
    db.apply_sql("CREATE DOMAIN d AS int; CREATE TABLE t (c d[]);")
        .unwrap();
    let element = domain("public", "d", int4());
    for (sql, expected) in [
        ("SELECT c AS a FROM t", array_of(element.clone())),
        (
            "SELECT ARRAY[1::d] AS a",
            array_with_elems(element.clone(), false),
        ),
        ("SELECT '{1}'::d[] AS a", array_of(element.clone())),
    ] {
        let q = db.analyze(sql).unwrap();
        assert_eq!(q.columns[0].pg_type, expected, "{sql}");
    }
}
