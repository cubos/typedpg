# typedpg

Compile-time verified PostgreSQL queries for Rust. **PostgreSQL only** -- no abstraction layer, no lowest-common-denominator SQL.

Write plain SQL, get full type safety. The `sql!` macro statically analyzes every query against your migration files during `cargo build` -- column types, parameter types, nullability, and type coercion errors are all caught before your code ships.

## Features

- **Compile-time checked** -- every query is validated against your actual schema. Typos in column names, wrong parameter types, invalid SQL, and type mismatches are all compiler errors. The analyzer reads your migrations and builds the schema in-memory -- no external process needed.
- **Real SQL** -- any syntax PostgreSQL accepts, `typedpg` accepts. CTEs, window functions, lateral joins, `DISTINCT ON`, `RETURNING`, `FOR UPDATE` -- if Postgres can parse and execute it, the macro will verify it. No restricted SQL subset, no Rust DSL.
- **Nullability-aware** -- the analyzer tracks nullability through JOINs, COALESCE, CASE, subqueries, and aggregates. `NOT NULL` columns become `T`, nullable columns become `Option<T>`.
- **Static type analysis** -- parameter types are inferred following PostgreSQL's own type resolution rules (operator/function resolution, implicit/assignment casts, common-type resolution). Type mismatches produce clear errors at compile time.
- **Zero runtime overhead** -- the macro generates concrete Rust structs with named fields. No runtime reflection, no `Box<dyn Any>`, no string-based column access.
- **PostgreSQL-native** -- first-class support for JSONB domains (`CREATE DOMAIN ... AS JSONB` mapped to Rust structs), enums (`CREATE TYPE ... AS ENUM` mapped to Rust enums), arrays, composite types, and advisory locks.

> **TypeScript** -- the same analyzer types `sql("...")` queries in TypeScript: install `@cubos/typedpg` as a dependency and `@cubos/typedpg-cli` as a devDependency (for the `typedpg` command and the bundler plugins), then `npx typedpg gen` generates, from your sources and migrations, the module that types and runs them on node-postgres or postgres.js. See [`@cubos/typedpg`](https://github.com/cubos/typedpg/blob/main/typedpg_npm/README.md).

## Quick start

Add to your `Cargo.toml`:

```toml
[dependencies]
typedpg = "0.7"
deadpool-postgres = "0.14"
tokio-postgres = "0.7"
tokio = { version = "1", features = ["full"] }

[package.metadata.typedpg.database]
migrations = "./migrations"
```

Create `migrations/0001_create_users.sql`:

```sql
CREATE TABLE users (
    id    SERIAL PRIMARY KEY,
    name  TEXT NOT NULL,
    email TEXT NOT NULL UNIQUE,
    age   INT,
    bio   TEXT
);
```

Use it:

```rust
let users = sql!(pool, "SELECT id, name, age FROM users")
    .fetch_all()
    .await?;

for user in &users {
    // user.id  : i32       (NOT NULL column → plain type)
    // user.name: String    (NOT NULL column → plain type)
    // user.age : Option<i32>  (nullable column → Option)
    println!("{}: {}", user.id, user.name);
}
```

The macro reads your migration files, builds the schema in memory, and type-checks the query. The generated struct has correctly typed fields with proper nullability.

Editing or deleting a migration re-runs the analysis: `sql!` records the migration files it read as dependencies of the generated code (the way `include_str!` does), so Cargo rebuilds the crate — and the macro with it — when one changes. The migrations' contents stay out of the binary; they land in the crate's metadata, once per crate.

**Optional:** add a `build.rs` (with `typedpg` also under `[build-dependencies]`) so that *adding* a migration file re-runs the analysis too — Cargo watches the directory, which a file dependency cannot:

```rust
// build.rs
fn main() {
    typedpg::build::track_migrations();
}
```

Without it everything still works — you just have to invalidate the build manually after adding a migration.

## Compile-time error detection

Errors in your SQL are caught at build time, not at runtime:

```rust
// Column doesn't exist → compile error
sql!(pool, "SELECT nonexistent FROM users").fetch_all().await?;
//  error: column "nonexistent" does not exist

// Wrong type in WHERE → compile error
sql!(pool, "SELECT id FROM users WHERE name").fetch_all().await?;
//  error: type mismatch: text cannot be coerced to bool

// Wrong type in LIMIT → compile error
sql!(pool, "SELECT id FROM users LIMIT true").fetch_all().await?;
//  error: type mismatch: bool cannot be coerced to int8

// Parameter type mismatch → compile error (expects i32 for age column)
let flag: bool = true;
sql!(pool, "UPDATE users SET age = $flag").execute().await?;
//  error: type mismatch: bool cannot be coerced to int4
```

## Linting

`cargo typedpg lint` analyzes every `sql!` in the crate's `src/`, `tests/`,
`examples/` and `benches/` against the migrations it is compiled against,
and lints the migrations themselves. Each rule has a page in
[`docs/lint`](docs/lint/README.md) with its motivation, what it proves,
examples and configuration:

| Rule | Default | What it reports |
|---|---|---|
| [`null_comparison`](docs/lint/null_comparison.md) | error | A comparison with a `NULL` literal, which is never true. |
| [`unbounded_write`](docs/lint/unbounded_write.md) | error | An `UPDATE` or `DELETE` with no `WHERE`. |
| [`not_in_nullable`](docs/lint/not_in_nullable.md) | warning | `NOT IN (SELECT …)` where either side may be NULL, which returns no rows. |
| [`create_index_blocking`](docs/lint/create_index_blocking.md) | warning | `CREATE INDEX` without `CONCURRENTLY` on a table that already has rows. |
| [`constraint_validating`](docs/lint/constraint_validating.md) | warning | `ADD CONSTRAINT` / `ADD COLUMN` validating every row under a lock. |
| [`set_not_null_scan`](docs/lint/set_not_null_scan.md) | warning | `ALTER COLUMN … SET NOT NULL` scanning the table. |
| [`table_rewrite`](docs/lint/table_rewrite.md) | warning | `ALTER TABLE` rewriting every row under an ACCESS EXCLUSIVE lock. |
| [`fk_without_index`](docs/lint/fk_without_index.md) | warning | A foreign key whose referencing columns have no index. A partial index over exactly those columns `WHERE col IS NOT NULL` counts: the foreign-key lookup's equality implies the predicate. |
| [`redundant_index`](docs/lint/redundant_index.md) | info | An index another index on the table already covers. |
| [`redundant_binding`](docs/lint/redundant_binding.md) | warning | A `sql!` binding `x = x` (or a bare `x`): the parameter captures the variable without it. |
| [`renaming_binding`](docs/lint/renaming_binding.md) | info | A `sql!` binding `p = v` that only renames the variable `v`; write `$v` in the SQL instead. |
| [`redundant_null_check`](docs/lint/redundant_null_check.md) | warning | `IS [NOT] NULL` or `COALESCE` on a value that is never NULL. |
| [`limit_without_order`](docs/lint/limit_without_order.md) | warning | `LIMIT`, `OFFSET` or `DISTINCT ON` without `ORDER BY`, which picks arbitrary rows. |
| [`redundant_limit`](docs/lint/redundant_limit.md) | info | A `LIMIT` on a query that returns at most one row. |
| [`redundant_distinct`](docs/lint/redundant_distinct.md) | info | A `DISTINCT` on rows that are already distinct. |
| [`constant_condition`](docs/lint/constant_condition.md) | warning | A `WHERE`, `JOIN … ON`, `HAVING` or `FILTER` condition — or an `AND` / `OR` / `NOT` part of one, or of a `CASE WHEN` — that is always true or never true: one the planner folds to a constant (`1 = 1`), or one what the analyzer knows decides — a column's `CHECK` constraints, domain or enum and the conditions before it (`status = 'actve'` when `status` is `CHECK (status IN ('active', 'inactive'))`, `n < 0` under `CHECK (n >= 0)`, `a = 2` after `a = 1 AND`, `x = x` on a column never NULL, an `EXISTS` whose subquery always returns a row). The message says why; a column of a table with a `NOT VALID` `CHECK` is not judged. The fix removes a `WHERE` the planner folds to true from a `SELECT`. |
| [`unreachable_case_branch`](docs/lint/unreachable_case_branch.md) | warning | A `CASE` branch that is never taken: a `WHEN` that is never true, or what follows one that always is — folded by the planner, or decided by what the analyzer knows there (the constraints, the conditions around the `CASE`, the `WHEN`s before it). |
| [`case_without_else`](docs/lint/case_without_else.md) | warning | A `CASE` without `ELSE` whose `WHEN`s may all fail, so it is NULL then; a `CASE` the constraints prove exhaustive is not reported. The fix writes `ELSE NULL`. |
| [`redundant_cast`](docs/lint/redundant_cast.md) | info | A cast to the type (and type modifier) its operand already has. |
| [`redundant_alias`](docs/lint/redundant_alias.md) | info | An output column alias that is the name the column already has (`SELECT name AS name`). |
| [`outer_reference_in_subquery`](docs/lint/outer_reference_in_subquery.md) | warning | An unqualified column in a subquery that no relation of the subquery has, so it is the outer query's (`WHERE id IN (SELECT id FROM b)` with no `b.id`). |
| [`cartesian_join`](docs/lint/cartesian_join.md) | warning | FROM items no `WHERE` / `ON` condition connects, directly or through other items, so every row pairs with every row; an explicit `CROSS JOIN`, a function in FROM, a `LATERAL` item referencing its left and an item of at most one row are not reported. |
| [`insert_without_columns`](docs/lint/insert_without_columns.md) | warning | An `INSERT INTO t VALUES …` / `SELECT …` without a column list, which depends on the table's column order. The fix writes the columns it targets now. |
| [`self_assignment`](docs/lint/self_assignment.md) | warning | An `UPDATE … SET a = a`, which changes nothing. The fix removes it when another assignment remains and no `UPDATE OF a` trigger fires on it. |
| [`unused_cte`](docs/lint/unused_cte.md) | warning | A `WITH` query (not a data-modifying one) nothing references. The fix removes it. |
| [`simplifiable_case`](docs/lint/simplifiable_case.md) | info | `CASE WHEN x IS NULL THEN y ELSE x END`, which is `COALESCE(x, y)`, and `CASE WHEN c THEN true ELSE false END`, which is `c` when `c` is never NULL. The fix rewrites it. |
| [`like_unescaped_param`](docs/lint/like_unescaped_param.md) | off | A `LIKE` pattern built from a parameter whose `%` and `_` are not escaped. |
| [`redundant_join`](docs/lint/redundant_join.md) | info | A join that can't change the rows and whose table nothing else reads: a `LEFT JOIN` whose `ON` equates every column of a full unique key of the joined table with columns of the same type (each row matches at most one row), or an inner `JOIN` over exactly a NOT NULL, validated, `NOT DEFERRABLE` foreign key into a parent without row-level security (each row matches exactly one). Only a plain `SELECT`'s joins to a table are read; a join whose removal would change a column's refinement is kept. The fix deletes the join. |
| [`scalar_subquery_may_return_many`](docs/lint/scalar_subquery_may_return_many.md) | warning | A subquery used as a value (`(SELECT x FROM t WHERE …)` in a select list, a comparison, a `SET`, a function argument) that may return more than one row, which fails at run time with "more than one row returned by a subquery used as an expression". A subquery proven to return at most one row (an aggregate without `GROUP BY`, a unique key pinned by its `WHERE`, outer references included, `LIMIT 1`) is not reported, nor are `IN`, `EXISTS`, `ANY` / `ALL` and `ARRAY(…)` subqueries, nor a subquery over a view, a CTE, a function, a FROM subquery or a table with an expression or partial unique index, which the proof can't see through. |
| [`timestamp_timezone_mix`](docs/lint/timestamp_timezone_mix.md) | warning | An implicit conversion between `timestamp` (or `date`) and `timestamptz` — a comparison (`created_at > now()` over a `timestamp` column), arithmetic, a function argument, a `CASE` / `COALESCE` / `UNION` type resolution, a value assigned to a column of the other type — whose result depends on the session's `TimeZone`. An explicit cast or `AT TIME ZONE`, a parameter and an untyped literal are not reported. |
| [`integer_division`](docs/lint/integer_division.md) | warning | An integer division, which truncates, whose result is then used as a `numeric` or floating-point value: `(done / total)::numeric`, `done / total * 100.0`, `done / total > 0.5`, `round(done / total, 2)`, or an integer product of it so used (`(done / total * 100)::numeric`). Convert an operand first: `done::numeric / total`. |
| [`left_join_nullified`](docs/lint/left_join_nullified.md) | warning | A `WHERE` condition never true for the null-extended rows of an outer join's nullable side (`LEFT JOIN b … WHERE b.status = 'x'`, `b.id IS NOT NULL`), so the join behaves as an inner join; `b.id IS NULL` (the anti-join), non-strict tests like `COALESCE(b.x, 0) = 0`, an `OR` with an arm keeping the rows and a join that never null-extends (a foreign key's parent) are not reported. |
| [`check_violation`](docs/lint/check_violation.md) | warning | An `INSERT … VALUES`, `UPDATE … SET` or `ON CONFLICT DO UPDATE SET` storing a constant a `CHECK` constraint (`NOT VALID` ones included) or the column's domain rejects whatever the rest of the row, so the statement fails for every row it writes; not reported when a `BEFORE` row trigger or a rule may change the row. |
| [`unstable_order`](docs/lint/unstable_order.md) | warning | An `ORDER BY` under `LIMIT`, `OFFSET` / `FETCH` or `DISTINCT ON` whose keys don't determine a unique row — no unique key (NOT NULL, or `NULLS NOT DISTINCT`) of every table is covered, counting columns `WHERE` pins to a constant, tables joined on their key and grouping keys — so tied rows come back in an arbitrary order and pages may skip or repeat rows. The help names the primary key to add as a tiebreaker. Queries returning at most one row, `WITH TIES`, `SKIP LOCKED` queues and FROM clauses the proof can't see through (subqueries, CTEs, partial or expression unique indexes) are not reported. |
| [`redundant_order_by`](docs/lint/redundant_order_by.md) | info | `ORDER BY` keys that can't change the order: a key after keys that already determine a unique row (the proof `unstable_order` uses), a column `WHERE` pins to a constant or parameter (`WHERE status = 'a' ORDER BY status, id`), a column an earlier key already sorts by, and the whole `ORDER BY` of a query returning at most one row or of an `EXISTS` / `IN` / `ANY` / `ALL` subquery (without `LIMIT`, `OFFSET`, `DISTINCT ON` or a locking clause), whose order nothing observes. `FROM` subqueries, CTEs and `ARRAY(…)` are not judged, nor a `DISTINCT ON`'s leading keys, nor a last key under `LIMIT` / `OFFSET` (that is `unstable_order`'s). The fix removes plain-column, output-name and ordinal keys (the whole clause when all go). |
| [`redundant_group_by`](docs/lint/redundant_group_by.md) | info | `GROUP BY` keys that can't split a group: a column of a table whose other grouped columns cover a unique NOT NULL key of it, or one `WHERE` pins to a constant or parameter. A key the select list, `HAVING`, `ORDER BY` or a window reads is only reported when the table's primary key stays grouped (PostgreSQL accepts nothing else for an ungrouped column), one key always stays, and `GROUPING SETS` / `ROLLUP` / `CUBE` are never touched. The fix removes the keys. |
| [`count_not_null`](docs/lint/count_not_null.md) | info | `count(expr)` over a value never NULL — a NOT NULL column no outer join null-extends, a literal (`count(1)`), a column the call's `FILTER` proves non-NULL — which counts every row like `count(*)`. `DISTINCT`, row values and parameters are not reported, nor an aggregate of outer columns (evaluated in the outer query). The fix writes `count(*)` when the argument is a plain column or constant. |
| [`non_sargable_predicate`](docs/lint/non_sargable_predicate.md) | info | A `WHERE` / `JOIN … ON` comparison (`=`, `<`, `<=`, `>`, `>=`, `BETWEEN`, `IN`, `= ANY`) that wraps an indexed column so no index can serve it: a converting cast (`created_at::date = $1`, `id::text = $1`), a function (`lower(email) = $1`, `date_trunc('day', ts) = $1`), arithmetic (`id + 1 = $1`) or an implicit conversion the other side's type forces (an `int4` column compared with a `numeric` value). Reported only when the column leads a non-partial btree index and its table has no expression index; the help names the range rewrite for `::date` and `date_trunc`. |
| [`float_equality`](docs/lint/float_equality.md) | warning | An `=`, `<>`, `IN` or `NOT IN` comparison made between `real` / `double precision` values (after implicit conversions), which rarely holds for computed values (`0.1 + 0.2 = 0.3` is false). A comparison with a value exact in binary floating point — an integer or decimal literal the type holds exactly (`0`, `0.5`, `1e3`; up to 2^24 for `real`), `'NaN'`, `'Infinity'` — is not reported; columns, parameters and joins (`a.f = b.f`) are. |
| [`fk_type_mismatch`](docs/lint/fk_type_mismatch.md) | warning | A foreign key whose column types differ from the referenced columns. |
| [`missing_lock_timeout`](docs/lint/missing_lock_timeout.md) | warning | A migration taking a blocking lock on an existing table without `SET lock_timeout`. |
| [`drop_index_blocking`](docs/lint/drop_index_blocking.md) | warning | `DROP INDEX` without `CONCURRENTLY` on an existing table, which takes an ACCESS EXCLUSIVE lock on it. |
| [`blocking_rebuild`](docs/lint/blocking_rebuild.md) | warning | `REINDEX` without `CONCURRENTLY`, `VACUUM FULL`, `CLUSTER` or `REPACK` without `CONCURRENTLY` on an existing table, which hold their lock for the whole rebuild. |
| [`detach_partition_blocking`](docs/lint/detach_partition_blocking.md) | warning | `DETACH PARTITION` without `CONCURRENTLY` on an existing partitioned table. |
| [`validate_in_same_transaction`](docs/lint/validate_in_same_transaction.md) | warning | `VALIDATE CONSTRAINT` in the transaction that added the constraint `NOT VALID`, while `ADD CONSTRAINT`'s lock is still held. |
| [`attach_partition_scan`](docs/lint/attach_partition_scan.md) | warning | `ATTACH PARTITION` of an existing table whose validated `CHECK` and `NOT NULL` constraints don't imply the partition bound (proven as PostgreSQL proves it; a `CHECK` alone doesn't make the key `NOT NULL`), so PostgreSQL scans it under lock to check the bound; also the scan of an existing default partition. |
| [`missing_primary_key`](docs/lint/missing_primary_key.md) | warning | A table without a primary key; partitions (which get their parent's) and temporary tables are skipped. |
| [`rls_policy_not_enforced`](docs/lint/rls_policy_not_enforced.md) | error | Row-level security policies on a table whose row-level security is not enabled: they do nothing. |
| [`rls_no_policy`](docs/lint/rls_no_policy.md) | info | Row-level security enabled on a table with no policy, which denies every role but the owner, superusers and `BYPASSRLS` roles. |
| [`security_definer_search_path`](docs/lint/security_definer_search_path.md) | warning | A `SECURITY DEFINER` function or procedure without `SET search_path`, which resolves names along the caller's search path; a SQL-standard (`BEGIN ATOMIC`) body, resolved at creation, is not reported. |
| [`prefer_timestamptz`](docs/lint/prefer_timestamptz.md) | off | A `timestamp without time zone` column. |
| [`prefer_identity`](docs/lint/prefer_identity.md) | off | A `serial` column instead of an identity column. |
| [`prefer_text`](docs/lint/prefer_text.md) | off | A `varchar(n)` or `char(n)` column instead of `text`. |
| [`prefer_bigint_pk`](docs/lint/prefer_bigint_pk.md) | off | An `int2` or `int4` primary key. |
| [`prefer_jsonb`](docs/lint/prefer_jsonb.md) | off | A `json` column, which has no equality operator (`DISTINCT`, `GROUP BY`, `UNION` fail), instead of `jsonb`. |
| [`fetch_may_return_many`](docs/lint/fetch_may_return_many.md) | warning | `fetch_one` / `fetch_one_as` / `fetch_value` / `fetch_optional` / `fetch_optional_as` / `fetch_value_optional` (`fetchOne` / `fetchValue` / `fetchOptional` / `fetchValueOptional` in TypeScript) on a query not proven to return at most one row. |
| [`fetch_all_single_row`](docs/lint/fetch_all_single_row.md) | info | `fetch_all` / `fetch_all_as` (`fetchAll` in TypeScript) on a query that returns at most one row. |
| [`unused_returning`](docs/lint/unused_returning.md) | info | `execute()` on a statement whose `RETURNING` rows are discarded. |
| [`breaks_deployed_queries`](docs/lint/breaks_deployed_queries.md) | warning | With `--previous-deployment <ref>` only: a migration that breaks a query of the code at that ref, which keeps running while the migration is applied. See below. |

The call-site rules (`fetch_may_return_many`, `fetch_all_single_row` and
`unused_returning`) run in both CLIs: a query can be called directly on the
`sql!(…)` / `sql(…)` expression, through a local binding, or — in TypeScript
— through an exported query of a `.sql` file or another source. Their findings
are placed at the method identifier in the calling file, and their fix edits
the SQL where the query is written. `redundant_binding` and
`renaming_binding` are about the `sql!` invocation in Rust, not the SQL text:
only `cargo typedpg lint` reports them (the TypeScript CLI has no bindings),
and only `[package.metadata.typedpg.lint]` accepts their keys.

The `lint` table sets each rule's level (`"error"`, `"warning"`, `"info"` or
`"off"`) and `fail_on`, the severity the run fails on (default `"error"`):

```toml
[package.metadata.typedpg.lint]
fail_on = "warning"
null_comparison = "off"
redundant_index = "warning"
ignore = ["tests/**"]   # paths whose statement findings are not reported
```

`ignore` lists glob patterns relative to the manifest's directory, with
forward slashes. Findings in a matching query file are dropped. A matching
migration file is still applied and its schema rules still report — on the
statement that created the object — but its statement rules are skipped, like
a migration unchanged from `--previous-deployment`: its history cannot be
changed. Failures are never dropped: a query in an ignored file that does not
compile still fails lint, as it still fails the build.

A comment suppresses a rule for the query or migration statement it is in:
`-- typedpg-lint: allow(unbounded_write)` (the `/* … */` form works too).

`--fix` applies the fixes of the findings that have one. It deletes a
`redundant_binding` (and, for a `renaming_binding`, rewrites every `$p` of
the SQL literal to `$v`), and applies the SQL edits some rules offer:
`redundant_null_check`'s `COALESCE`, `redundant_limit`,
`redundant_distinct`, `constant_condition`, `case_without_else`,
`redundant_cast`, `redundant_alias`, `insert_without_columns`,
`self_assignment`, `unused_cte`, `simplifiable_case`, `redundant_join` and
`unused_returning`. It writes the files, then
reports what remains; only edits that keep the query's meaning are applied.
Each fix is checked once more before it is offered: the edited query must
analyze to the same columns and parameters. A fix that doesn't is a typedpg
bug; it is withheld, and an `internal_error` warning asks you to report it.

`cargo typedpg lint --previous-deployment <ref>` names the git ref of what is
deployed now (for example `origin/main`): its code and its migrations.
Migrations already applied in production cannot be changed, so it skips the
statement rules of every migration file identical to its version at that ref:
it is applied for the schema, but its locks, scans and rewrites are not
reported. The schema rules (`fk_without_index`, `redundant_index`) still
report problems of the current schema, placed on the statement that created
the object, with a help line that puts the fix in a new migration; its allow
comments are honored and validated. A file absent from the ref, or changed by
the branch, is linted as usual. The summary states how many files skipped the
statement rules, e.g.
`300 unchanged from origin/main: statement rules skipped`.
(`--ignore-migrations-from`, its former name, still works.)

A deploy applies the new migrations while the code at the ref keeps running,
and it keeps running until the rollout ends. `breaks_deployed_queries`
analyzes that code's queries — read from the ref, so a query the branch
changed or removed is still checked — against both schemas, and reports each
one that analyzed against the old schema but breaks against the new: it no
longer analyzes (PostgreSQL's error is quoted), a result column is gone,
changes type or may now be NULL, the result's column list changes, a
parameter changes type or no longer accepts NULL, an `INSERT … SELECT` leaves
out a column that is now `NOT NULL` without a default, or a result value may
now be one the code doesn't know (a new enum label, a `CHECK` that admits
more values or a wider range). An `INSERT` (its `ON CONFLICT DO UPDATE SET`
too) or `UPDATE` breaks when a column it writes accepts fewer values: a
shorter `varchar(n)`, `char(n)` or `bit(n)`, a `numeric` with fewer digits
before the decimal point (or after it: the value is rounded), or a `CHECK` of
the table or the column's domain that is new or narrower, `NOT VALID` ones
included; a new `CHECK` over several columns that it writes may reject its
rows. A new `UNIQUE` constraint is not reported (it depends on the data), nor
is a length or precision change of a column it only reads. Each query is
analyzed as the roles it runs as — the call site's `role = "…"`, or the
database's `roles` — so a revoked privilege, or row-level security that now
leaves the role no policy, is reported too. The finding is placed on the migration
statement that makes the change and names the query's file and line at the
ref. The fix is expand/contract: first deploy code that works with both
schemas, then the migration. A `-- typedpg-lint:
allow(breaks_deployed_queries)` comment on the statement, or in the deployed
query, suppresses it.

Findings go to stdout, the summary to stderr, and the run exits non-zero when
a finding reaches `fail_on` or a query or migration fails analysis. Human is
the default format; `--format json` prints one JSON document and `--format
sarif` prints SARIF 2.1.0, both with the analysis failures included (the
human format keeps them on stderr). In CI:

```yaml
- run: cargo install typedpg_cli --locked
- run: cargo typedpg lint --format sarif > lint.sarif
- uses: github/codeql-action/upload-sarif@v4
  if: always()
  with:
    sarif_file: lint.sarif
```

## The `sql!` macro

Four terminal methods for different use cases:

```rust
// fetch_all -- returns Vec<Row>
let users = sql!(pool, "SELECT id, name FROM users")
    .fetch_all().await?;

// fetch_one -- returns a single Row (errors if empty or >1)
let user = sql!(pool, "SELECT id, name FROM users WHERE id = $id", id = 1)
    .fetch_one().await?;

// fetch_optional -- returns Option<Row>
let maybe = sql!(pool, "SELECT id, name FROM users WHERE id = $id", id = 42)
    .fetch_optional().await?;

// execute -- returns u64 (affected rows)
let n = sql!(pool, "DELETE FROM users WHERE id = $id", id = 1)
    .execute().await?;
```

### `fetch_value` -- single-column shortcut

When your query returns a single column, `fetch_value` and `fetch_value_optional` extract the scalar directly -- no struct wrapping:

```rust
// Returns i64 directly, not a struct with a `count` field
let count = sql!(pool, "SELECT count(*) FROM users")
    .fetch_value().await?;
// count: i64

// Returns Option<String>
let name = sql!(pool, "SELECT name FROM users WHERE id = $id", id = 42)
    .fetch_value_optional().await?;
// name: Option<String>

// Also works with aggregates — nullable when no GROUP BY
let max_age = sql!(pool, "SELECT max(age) FROM users")
    .fetch_value().await?;
// max_age: Option<i32>
```

`fetch_value` is only generated when the query returns exactly one column. Multi-column queries use `fetch_one`/`fetch_all` with struct access.

### `fetch_stream` -- rows as they arrive

For large results, `fetch_stream` yields the typed rows as the server sends them instead of collecting a `Vec` first (`fetch_stream_as::<T>()` maps them to your own `FromRow` type):

```rust
use typedpg::stream::TryStreamExt;

let mut users = sql!(pool, "SELECT id, name FROM users")
    .fetch_stream().await?;
while let Some(user) = users.try_next().await? {
    println!("{} {}", user.id, user.name);
}
```

The stream is a nameable `typedpg::QueryStream<T>` (`Send` and `Unpin`). Run on a pool, it holds its connection until it is dropped; an error raised while the server produces rows (say, a division by zero in row 3) comes through the stream.

## Named parameters

Parameters use `$name` syntax. Values can be explicitly assigned or captured from scope:

```rust
// Explicit assignment
sql!(pool, "SELECT id FROM users WHERE email = $email", email = "alice@example.com")
    .fetch_one().await?;

// Scope capture — if a variable named `email` exists, just use $email
let email = "alice@example.com";
sql!(pool, "SELECT id FROM users WHERE email = $email")
    .fetch_one().await?;
```

Parameter types are inferred from context, following PostgreSQL's rules:

```rust
// $name → String (inferred from users.name column type)
// $min_age → i32 (inferred from users.age column type)
// $limit → i64 (LIMIT requires bigint)
sql!(pool,
    "SELECT id, name FROM users WHERE name = $name AND age > $min_age LIMIT $limit",
    name = "Alice", min_age = 18, limit = 10)
    .fetch_all().await?;
```

## Nullability

The analyzer tracks nullability precisely through the entire query:

```rust
// NOT NULL columns → plain types
let user = sql!(pool, "SELECT id, name FROM users WHERE id = $id", id = 1)
    .fetch_one().await?;
// user.id   : i32
// user.name : String

// Nullable columns → Option
let user = sql!(pool, "SELECT id, age, bio FROM users WHERE id = $id", id = 1)
    .fetch_one().await?;
// user.id  : i32
// user.age : Option<i32>   (age is nullable)
// user.bio : Option<String> (bio is nullable)

// LEFT JOIN makes the right side nullable
let row = sql!(pool,
    "SELECT u.name, p.title
     FROM users u LEFT JOIN posts p ON p.user_id = u.id
     WHERE u.id = $id", id = 1)
    .fetch_one().await?;
// row.name  : String         (left side, NOT NULL)
// row.title : Option<String> (right side of LEFT JOIN → nullable)

// COALESCE removes nullability
let row = sql!(pool, "SELECT COALESCE(age, 0) AS age FROM users WHERE id = $id", id = 1)
    .fetch_one().await?;
// row.age : i32  (COALESCE with non-null fallback → NOT NULL)

// COUNT is never null, even without GROUP BY
let count = sql!(pool, "SELECT count(*) FROM users")
    .fetch_value().await?;
// count: i64

// But SUM/AVG/MAX without GROUP BY are nullable (empty table → NULL)
let total = sql!(pool, "SELECT sum(age) FROM users")
    .fetch_value().await?;
// total: Option<i64>
```

### Functions and aggregates

What a builtin does with NULL counts, not just whether it is strict: `format`,
`array_remove`, `string_to_array` and `||` on arrays are NULL only for the
arguments that make them so; `jsonb_each` always fills `key` and `value`;
`EXTRACT(epoch FROM interval)` stays non-NULL for an infinite interval. An
aggregate keeping NULL inputs (`array_agg`, `json_agg`, `JSON_ARRAYAGG`, …) is
NULL only over no rows, `rank(…) WITHIN GROUP` and `regr_count` never are, and
an aggregate has rows in a group, in a window frame holding the current row,
over a constant source (`VALUES`, `generate_series(1, 10)`) and in a group
HAVING keeps (`HAVING max(x) > 0`; `HAVING count(x) > 0` also makes `max(x)`
non-NULL).

```rust
// array_agg keeps NULLs: never NULL in a group, its elements may be
let rows = sql!(pool,
    "SELECT u.id, array_agg(p.title) AS titles
     FROM users u LEFT JOIN posts p ON p.user_id = u.id GROUP BY u.id")
    .fetch_all().await?;
// rows[0].titles : Vec<Option<String>>

// HAVING proves the row's group has a non-NULL age
let total = sql!(pool, "SELECT sum(age) FROM users HAVING count(age) > 0")
    .fetch_value_optional().await?;
// total: Option<i64>  (no row, or the sum — never a NULL sum)
```

### Narrowing by conditions

A condition that must hold for a value to be read narrows it, the way
PostgreSQL's planner reasons about strict operators (`find_nonnullable_vars`,
`reduce_outer_joins`): `WHERE`, `HAVING`, an inner join's `ON`, an
aggregate's `FILTER` and a `CASE` branch's `WHEN`.

```rust
// A strict WHERE condition proves its column non-NULL
let rows = sql!(pool, "SELECT age FROM users WHERE age > $min", min = 18)
    .fetch_all().await?;
// rows[0].age : i32

// ...and turns a LEFT JOIN that filters on its nullable side into an inner one
let rows = sql!(pool,
    "SELECT u.name, p.title FROM users u LEFT JOIN posts p ON p.user_id = u.id
     WHERE p.published_at IS NOT NULL")
    .fetch_all().await?;
// rows[0].title : String

// The same expression, filtered then selected
let rows = sql!(pool,
    "SELECT data ->> 'email' AS email FROM users WHERE data ->> 'email' IS NOT NULL")
    .fetch_all().await?;
// rows[0].email : String

// A CASE branch knows what its WHEN ruled out
let rows = sql!(pool, "SELECT CASE WHEN age IS NULL THEN 0 ELSE age END AS age FROM users")
    .fetch_all().await?;
// rows[0].age : i32

// An aggregate reads only the rows its FILTER passes
let rows = sql!(pool,
    "SELECT u.id, array_agg(p.title) FILTER (WHERE p.id IS NOT NULL) AS titles
     FROM users u LEFT JOIN posts p ON p.user_id = u.id GROUP BY u.id")
    .fetch_all().await?;
// rows[0].titles : Option<Vec<String>>  (no NULL element; NULL with no post)
```

The schema counts too, taking its constraints to hold as declared (`NOT VALID`
included; `NOT ENFORCED` ones say nothing):

```rust
// A LEFT JOIN along a NOT NULL foreign key always finds its row
let rows = sql!(pool,
    "SELECT p.title, u.name FROM posts p LEFT JOIN users u ON u.id = p.user_id")
    .fetch_all().await?;
// rows[0].name : String

// CHECK constraints combine with what the query knows
// CHECK ((kind = 'card' AND card_last4 IS NOT NULL) OR (kind = 'iban' AND iban IS NOT NULL))
let rows = sql!(pool, "SELECT card_last4, coalesce(card_last4, iban) AS ref FROM payments WHERE kind = 'card'")
    .fetch_all().await?;
// rows[0].card_last4 : String, rows[0].ref : String

// ...written as an iff, with NOT, IS DISTINCT FROM or CASE, and refuted by
// `<>`, IN lists, `= ANY (…)` and orderings (`lvl >= 10` against `lvl < 10`)
// CHECK ((status = 'done') = (done_at IS NOT NULL))
let rows = sql!(pool, "SELECT done_at FROM tasks WHERE status IN ('done')")
    .fetch_all().await?;
// rows[0].done_at : OffsetDateTime

// A CASE without ELSE is NOT NULL when its WHENs cover every value: an
// enum's labels, a CHECK (kind IN (…)) list (on the table or a domain), both
// booleans, the values a subquery or CTE column is known to hold (a CASE of
// literals), IS NULL and IS NOT NULL, `a > 0` and `a <= 0`
let rows = sql!(pool, "SELECT CASE status WHEN 'open' THEN 1 WHEN 'closed' THEN 2 END AS n FROM tickets")
    .fetch_all().await?;
// rows[0].n : i32  (status is a NOT NULL enum ('open', 'closed'))
```

A partition's bound counts as a constraint too (a range partition key, or a
list one with no NULL, is never NULL — in the partitioned table as well, when
no partition takes a NULL key), and so does a `MATCH FULL` foreign key: one of
its columns non-NULL makes all of them so. PostgreSQL has no NOT NULL for an
array's elements; `CHECK (array_position(tags, NULL) IS NULL)` on the column,
or on a domain over the array (`VALUE`), stands for one.

A referenced table under row-level security doesn't count: its policies may
hide the row. The foreign key holds just the same through subqueries, CTEs,
views and join trees that pass the key — or every row of the referenced table —
through unchanged, and for a scalar subquery that looks the referenced row up:

```rust
let rows = sql!(pool,
    "SELECT p.title, (SELECT u.name FROM users u WHERE u.id = p.user_id) AS author
     FROM posts p")
    .fetch_all().await?;
// rows[0].author : String
```

Some queries always yield a row: an aggregate without `GROUP BY` (or
`HAVING`), a query without `FROM`, `VALUES`, such a lookup. A scalar subquery
like that is NULL only when its value is, and an outer join `ON true` to one
never null-extends it:

```rust
let rows = sql!(pool,
    "SELECT u.name, s.posts FROM users u
     LEFT JOIN LATERAL (SELECT count(*) AS posts FROM posts p WHERE p.user_id = u.id) s ON true")
    .fetch_all().await?;
// rows[0].posts : i64
```

A `FULL JOIN` row has one side or the other, so `coalesce(x.id, y.id)` over a
NOT NULL column of each is NOT NULL; a strict condition on a column a subquery
or view passes through narrows the row it came from; and a row of a strict
set-returning function in `FROM` (`generate_series(1, t.n)`,
`jsonb_array_elements(t.doc)`) proves its arguments were non-NULL.

Besides strict conditions, any condition that can't hold with a column NULL
counts: `coalesce(age, 0) > 0`, `age IS NOT DISTINCT FROM 5`, `(tenant_id, id)
= ($1, $2)`, `b IS NULL OR b > 0` (never NULL itself), or an `EXISTS` whose
subquery's `WHERE` is strict in an outer column. A condition on an expression
narrows the same expression where it is read again — `WHERE data ->> 'k' IS
NOT NULL` then `data ->> 'k'`, `HAVING max(x) > 0` then `max(x)` — unless it
runs a volatile function. Conditions that may hold for a NULL prove nothing:
`coalesce(age, 1) > 0`, `age IS DISTINCT FROM 5`, or an `OR` whose arms test
different columns.

A generated column (STORED or VIRTUAL) is its expression over the row, so
`GENERATED ALWAYS AS (coalesce(note, ''))` is never NULL. A view that is
automatically updatable reads its base table's CHECK constraints.

`RETURNING` knows what the statement wrote. An `INSERT` returns its values
and the column defaults; an `UPDATE` returns its `SET` values and whatever
its `WHERE` proved about the columns it keeps. `ON CONFLICT DO UPDATE`
returns the inserted row or the updated one, and `MERGE` one row per action
it runs, past its `WHEN` condition. Every returned row satisfies the table's
CHECK constraints. An automatically updatable view writes its base table's
rows, which need not pass its `WHERE`. A BEFORE ROW trigger or a rule may
rewrite the row, so the values aren't trusted there (only the constraints
are), and a `DO INSTEAD` rule or an `INSTEAD OF` trigger returns rows of its
own, of which nothing is known:

```rust
// DEFAULT 'draft', and a SET of a non-NULL value
let row = sql!(pool, "INSERT INTO posts (title) VALUES ($t) RETURNING status", t = "Hi")
    .fetch_one().await?;
// row.status : String
let rows = sql!(pool, "UPDATE users SET age = 18 WHERE age IS NULL RETURNING age")
    .fetch_all().await?;
// rows[0].age : i32
```

A data-modifying CTE that inserts one `VALUES` row (without a set-returning
function, which makes it any number of rows, nor `ON CONFLICT DO NOTHING`, a
`DO UPDATE ... WHERE` or a BEFORE ROW trigger, which may skip it) returns
exactly one row, so `(SELECT id FROM ins)` is that row's `id`.

### Finite dates and times

`to_char` and `EXTRACT` / `date_part` of most fields are NULL for an infinite
date, timestamp or interval. The analysis knows a value is finite when it is
the current time (`now()`, `CURRENT_DATE`, …), a literal other than
`infinity`, a `date` / `timestamp` / `timestamptz` parameter (chrono's
types, which `sql!` binds them from, can't hold an infinity — unless
`[types]` maps the type), arithmetic, truncation or a conversion of finite
values, the `min` / `max` / `lag` of them, or a column whose table (or
domain) says so with `CHECK (isfinite(col))`. It follows the value through subqueries, CTEs,
`UNION`, `CASE` and `COALESCE`:

```rust
// CHECK (isfinite(created_at))
let rows = sql!(pool,
    "SELECT to_char(created_at, 'YYYY-MM') AS month,
            extract(day FROM now() - interval '1 week') AS day
     FROM posts")
    .fetch_all().await?;
// rows[0].month : String, rows[0].day : Decimal
```

### Values and ranges

The analysis also knows which values a value can be, when few — an enum's
labels, a `CHECK (kind IN (…))` list (on the table or a domain), what the
`WHERE` or a `CASE` branch leaves (`WHERE kind <> 'draft'`), literals — and
the bounds of an integer: a `CHECK (n BETWEEN 1 AND 100)` or `CHECK (VALUE >
0)`, a `WHERE`, counts and positions (`count(*)`, `row_number()`, `length`),
`min` / `max` and integer arithmetic. `NULLIF(x, v)` is NOT NULL when `x`
can't be `v`, and a `CASE` covering every value a column can be has no
`ELSE` to fall to:

```rust
// CHECK (attempts BETWEEN 0 AND 2)
let rows = sql!(pool,
    "SELECT total / NULLIF(row_number() OVER (), 0) AS avg,
            CASE attempts WHEN 0 THEN 'new' WHEN 1 THEN 'retried' WHEN 2 THEN 'last' END AS stage
     FROM jobs")
    .fetch_all().await?;
// rows[0].avg : i64, rows[0].stage : String
```

### Nullability annotations

Override the inferred nullability when you know better than the analyzer. Use `!` to force non-nullable and `?` to force nullable.

**Columns** -- append `!` or `?` to the alias:

```rust
// p.title comes from a LEFT JOIN, so it's inferred as Option<String>.
// But if you know the join always matches, force it with "!":
let row = sql!(pool,
    r#"SELECT u.name, p.title as "title!"
       FROM users u LEFT JOIN posts p ON p.user_id = u.id
       WHERE u.id = $id"#, id = 1)
    .fetch_one().await?;
// row.title : String  (forced NOT NULL)

// name is NOT NULL, but you can force it nullable with "?":
let row = sql!(pool, r#"SELECT name as "name?" FROM users WHERE id = $id"#, id = 1)
    .fetch_one().await?;
// row.name : Option<String>  (forced nullable)
```

**Parameters** -- append `!` or `?` to the parameter name:

```rust
// Inferred from target column:
// age is nullable → $age accepts Option<i32>
sql!(pool, "UPDATE users SET age = $age WHERE id = $id", age = Some(25), id = 1)
    .execute().await?;

// name is NOT NULL → $name requires String (not Option)
sql!(pool, "UPDATE users SET name = $name WHERE id = $id", name = "Alice", id = 1)
    .execute().await?;

// Override:
// name is NOT NULL, but $name? forces it to accept Option<String>
sql!(pool, "UPDATE users SET name = $name? WHERE id = $id",
    name = Some("Alice"), id = 1)
    .execute().await?;

// age is nullable, but $age! forces it to require i32 (not Option)
sql!(pool, "UPDATE users SET age = $age! WHERE id = $id",
    age = 25, id = 1)
    .execute().await?;
```

## Bulk insert with `$..spread`

Insert multiple rows in a single statement:

```rust
struct NewUser { name: String, email: String }

let new_users = vec![
    NewUser { name: "Alice".into(), email: "alice@example.com".into() },
    NewUser { name: "Bob".into(),   email: "bob@example.com".into() },
];

sql!(pool, "INSERT INTO users (name, email) VALUES $..new_users { name, email }")
    .execute().await?;
```

The macro expands `$..new_users { name, email }` into a multi-row `VALUES` clause with proper parameter numbering. With no item the query still runs, as SQL would with no row: an empty spread next to other rows is left out, and a `VALUES` of nothing but empty spreads — which PostgreSQL has no syntax for — is written as a `SELECT` of no row (`SELECT * FROM (VALUES (NULL::type, …)) AS __typedpg_empty WHERE false`). So inserts in other CTEs of the statement happen, an aggregate over it has its row, and statement-level triggers fire.

## Lists with `IN $..list`

A spread without fields, right after `IN`, expands a list of values, each typed by the `IN`'s left side:

```rust
let ids: Vec<i64> = vec![1, 2, 3];

sql!(pool, "SELECT name FROM users WHERE id IN $..ids")   // WHERE id IN ($1, $2, $3)
    .fetch_all().await?;
```

An empty list is written `(SELECT NULL::<type> WHERE false)` — PostgreSQL has no syntax for one — and the query runs: `x IN` it is false, `x NOT IN` it true.

Above 1000 items, a spread is bound as arrays where that is provably the same query — the analyzer analyzes the array form and offers it only if the columns and parameters come out identical: `x IN $..ids` is written `(x = ANY($1::type[]))` (`NOT IN`: `<> ALL`), and a `VALUES` made of one rows spread `SELECT * FROM unnest($1::type1[], …) AS __typedpg_rows (column1, …)`. Large lists and batches then take a parameter per field instead of one per value — faster, and past PostgreSQL's 65535-parameter limit. Not offered where the form would differ: a field that is an array (unnest would flatten it) or a composite, a `VALUES` with written rows, a column whose nullability the array would change, an enum or JSON-domain field whose values may be `None`.

`id = ANY($ids)`, with an array parameter, is the same filter as a single placeholder, whatever the list's length. `sql!` prepares each query anew, never reusing a statement, so PostgreSQL plans it with the values and the two plan alike. They differ only under a generic plan — a statement prepared once and executed repeatedly, as an executor of your own might — where PostgreSQL prunes the partitions of a partitioned table only for an `IN` list, and estimates its rows from the list's length instead of assuming 10 elements. An `IN` list is one statement text per length, and at most 65535 parameters.

## Bulk loading with `copy_in!`

For large loads, `copy_in!` streams rows through PostgreSQL's binary `COPY ... FROM STDIN` — much faster than `INSERT`, and with no limit on the number of rows:

```rust
use typedpg::copy_in;

let copied: u64 = copy_in!(pool, "users (name, email)", new_users { name, email })
    .await?;
```

- The target is a table and an optional column list (without one: every column but the generated ones), checked at compile time against your migrations — an unknown table or column, a generated column or a view without an `INSTEAD OF INSERT` trigger is a compile error with PostgreSQL's message.
- The rows are any `IntoIterator` (a `Vec`, `&slice`, or a lazy iterator); each item supplies the listed fields in the columns' order, typed by the columns — `Option<T>` only for nullable ones. Items are converted one at a time as the COPY consumes them.
- Enums, JSONB domains and arrays of them go through your `[package.metadata.typedpg.types]` mappings, as in `sql!`.
- If any row fails, the whole COPY is aborted and no row is kept. Inside a transaction, it commits or rolls back with it.
- It takes `sql!`'s `db = ` and `role = ` prefixes, and is checked against the configured roles like the `INSERT` it amounts to (see [Role checks](#role-checks)).

Use `$..spread` when you need `RETURNING` or `ON CONFLICT`; `copy_in!` when you just need the rows in.

## Enum types

PostgreSQL enums (`CREATE TYPE ... AS ENUM`) are supported out of the box. Without configuration, they map to `String`:

```sql
CREATE TYPE user_role AS ENUM ('admin', 'editor', 'viewer');

CREATE TABLE users (
    id   SERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    role user_role NOT NULL DEFAULT 'viewer'
);
```

```rust
// role is typed as String
let user = sql!(pool, "SELECT id, name, role FROM users WHERE id = $id", id = 1)
    .fetch_one().await?;
println!("Role: {}", user.role); // "admin", "editor", or "viewer"

// Parameters for enum columns also accept String
sql!(pool, "UPDATE users SET role = $role WHERE id = $id", role = "editor", id = 1)
    .execute().await?;
```

To get type-safe enum values instead of raw strings, map them in `Cargo.toml`:

```toml
[package.metadata.typedpg.types]
user_role = "crate::UserRole"
```

The macro will use your Rust type for serialization/deserialization. Your type must convert to/from `String`.

## Domain types (JSONB)

PostgreSQL domains (`CREATE DOMAIN`) are supported. A domain over `JSONB` can be mapped to a Rust struct that implements `serde::Serialize` and `serde::Deserialize`:

```sql
CREATE DOMAIN user_preferences AS JSONB;

CREATE TABLE profiles (
    user_id INT PRIMARY KEY REFERENCES users(id),
    preferences user_preferences
);
```

Without configuration, JSONB domains resolve to `serde_json::Value`:

```rust
// preferences: Option<serde_json::Value> (nullable JSONB domain)
let profile = sql!(pool, "SELECT user_id, preferences FROM profiles WHERE user_id = $id", id = 1)
    .fetch_one().await?;
```

With configuration, the macro automatically serializes/deserializes through your Rust type:

```toml
[package.metadata.typedpg.types]
user_preferences = "crate::domains::UserPreferences"
```

```rust
#[derive(Serialize, Deserialize)]
struct UserPreferences { theme: String, lang: String }

// preferences: Option<UserPreferences> -- automatic deserialization
let profile = sql!(pool, "SELECT user_id, preferences FROM profiles WHERE user_id = $id", id = 1)
    .fetch_one().await?;

if let Some(prefs) = &profile.preferences {
    println!("Theme: {}", prefs.theme);
}

// Parameters are also automatically serialized
let prefs = UserPreferences { theme: "dark".into(), lang: "pt-BR".into() };
sql!(pool, "UPDATE profiles SET preferences = $prefs WHERE user_id = $id", prefs = prefs, id = 1)
    .execute().await?;
```

Non-JSONB domains (e.g. `CREATE DOMAIN positive_int AS INT CHECK (VALUE > 0)`) are transparently unwrapped to their base type.

## PostgreSQL extensions

`CREATE EXTENSION` is processed by the DDL interpreter — the types, operators, and functions an extension installs become visible to the analyzer immediately. `citext`, `hstore`, `pg_trgm`, `uuid-ossp`, `btree_gin`, `vector` (pgvector), and the rest of the contrib bundle all work without extra setup.

For pgvector specifically, the macro auto-routes `vector` / `halfvec` / `sparsevec` columns to the [`pgvector`](https://docs.rs/pgvector) crate:

```sql
CREATE EXTENSION vector;

CREATE TABLE documents (
    id        SERIAL PRIMARY KEY,
    title     TEXT NOT NULL,
    embedding vector(384) NOT NULL
);
```

```rust
use pgvector::Vector;

// Top-5 nearest neighbours by cosine distance.
let query: Vector = embed("how do I write a CTE?");

let similar = sql!(pool,
    "SELECT id, title, embedding <=> $query AS distance
     FROM documents
     ORDER BY embedding <=> $query
     LIMIT $k",
    k = 5_i64)
    .fetch_all().await?;

for doc in &similar {
    // doc.id      : i32
    // doc.title   : String
    // doc.distance: f64               (`<=>` returns float8)
    // $query was inferred as pgvector::Vector from the operator's left operand
    println!("{}: {:.3}", doc.title, doc.distance);
}
```

The analyzer knows the `<=>`, `<->`, and `<#>` operators (and the matching index ops), so wrong operand types are caught at compile time just like for built-in types.

For extension types without a built-in mapping (e.g. `citext`, `hstore`), point them at the Rust type in `Cargo.toml`:

```toml
[package.metadata.typedpg.types]
"public.citext" = "String"
"public.hstore" = "::std::collections::HashMap<String, Option<String>>"
```

## Transactions

Pass a transaction directly to `sql!`:

```rust
let mut client = pool.get().await?;
let tx = client.transaction().await?;

sql!(&tx, "INSERT INTO users (name, email) VALUES ($name, $email)",
    name = "Charlie", email = "charlie@example.com")
    .execute().await?;

sql!(&tx, "UPDATE users SET name = $name WHERE email = $email",
    name = "Charles", email = "charlie@example.com")
    .execute().await?;

tx.commit().await?;
```

The `Executor` trait is implemented for:

| Type | Feature | Behavior |
|------|---------|----------|
| `deadpool_postgres::Pool` | `deadpool` (default) | Acquires a connection per query |
| `deadpool_postgres::Object` | `deadpool` (default) | Uses the pooled connection |
| `bb8::Pool<PostgresConnectionManager<Tls>>` | `bb8` | Acquires a connection per query |
| `tokio_postgres::Client` | always | Uses the raw client directly |
| `tokio_postgres::Transaction<'_>` | always | Executes within the transaction |

A query goes to the server in one round trip: `sql!` already knows the parameter types, so it sends them with the query (tokio-postgres's `query_typed`) instead of asking the server first. That covers built-in types (`int4`, `text`, `jsonb`, `timestamptz`, arrays of them, a domain over one, …), and also enums, domains over them and extensions' scalar types (`citext`, `vector`, …): their OIDs are the database's own, so they go as "unspecified" and the server takes the type from the cast `sql!` writes after the placeholder. An array of one of those, a composite, or a range of the database's own as a parameter — or, for the `fetch_*` methods, a result column of any type whose OID is the database's own — sends the query untyped, as tokio-postgres's `query` does: prepared first, one more round trip. An `Executor` of your own runs both through `query_stream` / `execute` unless it overrides `query_typed_stream` / `execute_typed`.

## Mapping rows to your own structs

By default `sql!` synthesises an anonymous result struct per query. When you want to return a struct you already own — say a domain type shared across several queries — derive `FromRow` and use the `fetch_*_as::<T>` methods:

```rust
#[derive(typedpg::FromRow)]
struct User {
    id: i32,
    name: String,
    email: Option<String>,
}

let user: User = sql!(pool, "SELECT id, name, email FROM users WHERE id = $id", id = 1)
    .fetch_one_as::<User>()
    .await?;

let users: Vec<User> = sql!(pool, "SELECT id, name, email FROM users")
    .fetch_all_as::<User>()
    .await?;

let maybe: Option<User> = sql!(pool, "SELECT id, name, email FROM users WHERE id = $id", id = 42)
    .fetch_optional_as::<User>()
    .await?;
```

Field names must match the query's output column names (a `"col!"` / `"col?"` alias names the column `col`). Each field's type must be its column's Rust type, or an `Option` of it, and this is checked at compile time: `age: i32` for a nullable `age`, or a field the query doesn't return, is a compile error pointing at the field. Rows are decoded the way `sql!` decodes its own struct, so mapped enum, JSONB-domain and composite fields work too.

## Multiple databases

When a single crate talks to more than one database, declare each one under `[package.metadata.typedpg.databases.<name>]` and pick which to use at the `sql!` site with a `db = <name>` prefix:

```toml
# Default database (used when `db = ...` is omitted)
[package.metadata.typedpg.database]
migrations = "./migrations"

# A second database, e.g. an analytics warehouse with its own schema
[package.metadata.typedpg.databases.analytics.database]
migrations = "./analytics/migrations"

[package.metadata.typedpg.databases.analytics.types]
"public.metric_kind" = "crate::analytics::MetricKind"
```

```rust
// Default database — schema comes from `./migrations`
let users = sql!(app_pool, "SELECT id, name FROM users")
    .fetch_all().await?;

// `db = analytics` switches the schema source to the named entry
let metrics = sql!(db = analytics, warehouse_pool, "SELECT kind, total FROM daily_metrics")
    .fetch_all().await?;
```

Each named entry is independent: its own migrations, its own `[migrations]` runner settings, its own `[types]` map. The runtime executor (`app_pool` / `warehouse_pool` above) is still passed by the caller — `typedpg` does not multiplex pools, it only points the compile-time analyzer at the right schema.

## Role checks

`sql!` can also prove that the role your application connects as may run each statement. PostgreSQL checks privileges when a statement executes, so a query that builds against the migrations can still fail in production with `permission denied`; listing the role moves that failure to the build:

```toml
[package.metadata.typedpg.database]
migrations = "./migrations"
roles = ["app_role"]
```

The migrations create the role and grant what the application is allowed to do:

```sql
CREATE ROLE app_role;
GRANT SELECT ON users TO app_role;
GRANT INSERT, UPDATE ON orders TO app_role;
```

Every `sql!` statement of that database is analyzed as `app_role` (after the owner's own analysis), and the first statement it may not run is a compile error, with PostgreSQL's message and the grant that would allow it:

```text
error: permission denied for table orders
  help: grant it in a migration: GRANT SELECT ON public.orders TO app_role;
  note: role app_role has no SELECT on public.orders
 --> src/orders.rs:12:14
```

The analysis makes the same checks PostgreSQL makes when the role prepares and executes the statement: USAGE on the schemas it names, SELECT/INSERT/UPDATE/DELETE on its relations — per column where that is what the role is granted — EXECUTE on the routines the plan runs, and the sequence privileges of `nextval` and of serial columns' defaults. It resolves unqualified names along the role's own search path (`ALTER ROLE … SET search_path`); a view is read as its owner unless it is `security_invoker`, and a write through an auto-updatable view is checked on the base table, as PostgreSQL does.

Row-level security is checked as PostgreSQL applies it: the policies that apply to the role for the command (permissive and restrictive, `USING` and `WITH CHECK` where the command uses each) run their functions as the role and their subqueries as the relation's reader, and a role whose `row_security` is off is refused where a policy would apply, as PostgreSQL refuses it.

The utility statements a query may be check what their command needs when they run, and so does the analysis: `TRUNCATE` needs TRUNCATE on every table it names or cascades to (and ownership of the sequences `RESTART IDENTITY` resets), and fires the tables' `TRUNCATE` triggers; `LOCK` needs a privilege its mode accepts (SELECT for `ACCESS SHARE`, INSERT up to `ROW EXCLUSIVE`, UPDATE, DELETE, TRUNCATE or MAINTAIN for any), on a view's tables too, as the view's owner; `REFRESH MATERIALIZED VIEW` needs MAINTAIN, and its query is checked as the view's owner, who runs it; `SET` of a parameter only a superuser sets — and a `set_config` call that runs and sets one — needs `GRANT SET ON PARAMETER`, and `SET ROLE` a membership granted `WITH SET`. `ANALYZE` skips (with a warning) what the role may not maintain, so it never fails for privileges — but a role that may maintain a foreign table whose wrapper has no handler hits PostgreSQL's `has no handler` error. What fails on every execution whatever the role — `TRUNCATE` of a table another one references without `CASCADE`, `REFRESH … CONCURRENTLY` of a view without a unique index, an unknown parameter or an invalid value — is a compile error for every role.

The routines and triggers a statement runs are checked too, recursively: a `SECURITY DEFINER` body runs as its owner, anything else as the role running the statement, and a body resolves its names along its own `SET search_path` or — without one — the path in effect at the call site (a `SECURITY DEFINER` body does not read the owner's `ALTER ROLE … SET search_path`). A statement is checked against everything it *may* run for some data: every trigger that may fire and every body statement that may be reached. Code that provably never runs is pruned: `IF` / `ELSIF` / `CASE` / `WHILE` conditions the planner certainly folds to a constant (immutable operators and functions over constants; never a domain coercion or a non-immutable cast), with the same certainty dropping `CASE` results, `COALESCE` arguments after a constant, the dead side of `AND` / `OR`, and the functions of stored `CHECK`s, column defaults, generated columns, index expressions and predicates, conditions on `TG_OP`, `TG_WHEN`, `TG_LEVEL`, `TG_TABLE_NAME`, `TG_TABLE_SCHEMA`, `TG_RELNAME` or `TG_NAME` evaluated for the known firing (an unknown `TG_*` is never NULL), statements after an unconditional `RETURN` / `RAISE EXCEPTION`, after a nested block whose list ends so, or after an `IF` whose every branch including `ELSE` ends, `DECLARE` defaults of blocks never entered, the body of a `BEGIN … EXCEPTION` block whose handler catches `insufficient_privilege`, SQLSTATE `'42501'` or `OTHERS` and doesn't re-raise — the handler's own statements are still checked, and a deferrable constraint trigger's denial at commit is not caught. A cursor's query is checked when the cursor is opened. A trigger whose `WHEN` condition certainly folds to false never fires; otherwise the condition's functions and operators are checked as the caller (they run per row) and its body is checked. A write through an auto-updatable view checks only the view's `WHERE` (under a `CHECK OPTION`, `CASCADED` reaching the views below), not its select list, and an inlined SQL function skips the arguments its body doesn't use. An `UPDATE` may be non-HOT for the data, so every index's expressions and predicate are checked. A row trigger on a partitioned table fires through its clones on the partitions, with each clone's own enabled state, and an `UPDATE` that may move a row between partitions — the SET list or a BEFORE row trigger may change a key column — fires the source partition's `DELETE` row triggers and the destination's `INSERT` ones. An SQL-standard body (`BEGIN ATOMIC … END` / `RETURN …`) is resolved when its function is created, as PostgreSQL stores it analyzed: it reads the objects the creation search path found, and running it checks no schema USAGE.

`roles` is per database (in `[package.metadata.typedpg.databases.<name>.database]` for a named one). A `role = "..."` prefix at the call site replaces the configured roles for that one statement; the two prefixes go in either order:

```rust
let recent = sql!(db = analytics, role = "reporting", warehouse_pool,
    "SELECT id, total FROM daily_metrics WHERE day > $day")
    .fetch_all().await?;
```

Only roles the migrations create can be checked: typedpg's catalog knows what those roles own and were granted, and nothing about the rest of the cluster. Naming another one is the compile error `role "x" does not exist (typedpg can check only the roles the migrations create)`.

### Keeping crates out of each other's tables

The same check keeps a workspace split by domain honest, even when every crate connects as one database role in production: give each crate a role of its own that exists only for the analysis. Put the roles and their grants in a directory the crates list in `extra_migrations` — files there build the compile-time schema but are never applied by the migration runner — and configure each crate's `roles`. Files from `migrations` and every `extra_migrations` directory are merged and applied in one order — by numeric prefix, then file name (what `typedpg_core::migrations::compare_migration_names` defines, and how the runner orders its own migrations) — so the role file is numbered after the migrations that create the tables it grants on:

```sql
-- analysis/9000_crate_roles.sql
CREATE ROLE placement;
GRANT pg_read_all_data TO placement;           -- reads anything
GRANT INSERT, UPDATE, DELETE ON machines TO placement;
-- `pg_read_all_data` covers SELECT only. A serial column's `nextval`
-- still needs USAGE on its sequence; an identity column's sequence comes
-- with the table's INSERT and needs no grant.
GRANT USAGE ON SEQUENCE machines_id_seq TO placement;

CREATE ROLE people;
GRANT pg_read_all_data TO people;
GRANT INSERT, UPDATE, DELETE ON users, projects TO people;

-- Exception: placement still sets projects.state until the lifecycle
-- crate takes it over (remove by 2027-03).
GRANT UPDATE (state) ON projects TO placement;
```

```toml
# placement/Cargo.toml
[package.metadata.typedpg.database]
migrations = "../migrations"
extra_migrations = ["../analysis"]
roles = ["placement"]
```

A write from `placement` to `users` is now a compile error naming the grant it would take. An exception is that grant, written in the file with its reason: it is reviewed like any other change, and removing it later brings the error back for every statement that still relies on it.

The bodies of the triggers a statement fires and of the routines it calls are checked too: a body runs with the statement's role, or with its owner's role under `SECURITY DEFINER`, and what it reads and writes must be allowed as well. Not checked yet: dynamic SQL in those bodies (`EXECUTE`) and statements typedpg can't analyze (a name it can't resolve, a construct it doesn't model).

`copy_in!` is checked like the `INSERT` it amounts to: INSERT on the columns it copies, the defaults of the others and the `INSERT` triggers it fires. Row-level security is the exception: PostgreSQL applies no policy to a `COPY FROM` and refuses it instead (`COPY FROM not supported with row-level security`), and so does the check.

## Migrations

### CLI

Install the CLI and manage migrations from the terminal:

```bash
cargo install typedpg_cli
```

```bash
# Create a new migration (generates .sql and .down.sql files)
cargo typedpg migrate create add_posts_table

# Apply all pending migrations
cargo typedpg migrate up

# Show migration status
cargo typedpg migrate status

# Revert the last applied migration
cargo typedpg migrate down

# Revert a specific migration
cargo typedpg migrate down 20260406120000_add_posts_table

# Force revert even without a .down.sql file
cargo typedpg migrate down --force

# Record an applied migration's edited file as applied, without running it
cargo typedpg migrate accept 20260406120000_add_posts_table
```

The CLI reads configuration from `[package.metadata.typedpg]` in your `Cargo.toml` and connects using the `DATABASE_URL` environment variable (supports `.env` files; a variable already set in the environment wins over the file).

Editing an applied migration's statements is drift: with `fail_on_drift` (the
default) the run aborts before applying anything new, and prints a unified diff
of the applied text against the file; with `fail_on_drift = false` it only
warns. Comments, whitespace and the case of unquoted words do not count — only
a change to the statements does. If an edit is intentional and does not need
to run again, `cargo typedpg migrate accept <name>` records the file as
applied without running it; otherwise restore the file and add a new
migration.

### Programmatic

You can also run migrations programmatically at application startup:

```rust
use typedpg::migrate::{MigrationSource, MigrationsConfig, run};
use std::path::Path;

let source = MigrationSource::from_dir(Path::new("./migrations"))?;
let config = MigrationsConfig::default();
let applied = run(&mut client, &source, &config).await?;
```

For binaries that should ship without their migration files on disk, use the `embed_migrations!` macro to bake them into the binary at compile time:

```rust
use typedpg::migrate::{MigrationsConfig, run};

// Path is resolved relative to the invoking crate's CARGO_MANIFEST_DIR,
// like `include_str!`. The SQL contents land in the final binary as &'static str;
// editing a migration file re-triggers a rebuild automatically.
let source = typedpg::embed_migrations!("./migrations");
let config = MigrationsConfig::default();
let applied = run(&mut client, &source, &config).await?;
```

Migrations use advisory locks for safe concurrent deploys and wrap each migration in a transaction by default (opt-out with `-- no-transaction` as the first line of a migration file).

## Configuration

All configuration lives in your `Cargo.toml`:

```toml
[package.metadata.typedpg.database]
migrations = "./migrations"        # required — path to migration files
extra_migrations = ["../shared/migrations"]  # optional — extra migration dirs
                                             # included in the compile-time
                                             # schema only (NOT applied at
                                             # runtime); use when another crate
                                             # owns shared tables
roles = ["app_role"]               # optional — roles every sql! statement
                                   # must be runnable by (see Role checks)

[package.metadata.typedpg.migrations]
table = "public._migrations"       # optional — migration tracking table
lock_id = 713705                   # optional — advisory lock ID
use_transaction = true             # optional — wrap each migration in a tx
fail_on_drift = true               # optional — abort if an already-applied
                                   # migration's statements have been edited
                                   # since (comments, whitespace and unquoted
                                   # case do not count)

[package.metadata.typedpg.types]       # optional — PG type → Rust type
user_preferences = "crate::UserPrefs"  # JSONB domain → Rust struct (serde)
user_role = "crate::UserRole"          # PG enum → Rust enum (Display + FromStr)
```

A key typedpg does not recognize is a compile error rather than silently
ignored.

### Query simplifications

Where the analysis proves an equivalent query returns the same rows in
every state the schema allows, `sql!` sends that one instead, because
PostgreSQL plans it better. The proofs rest only on what PostgreSQL
enforces on every statement (validated `NOT NULL`s, keys), not on CHECKs,
`NOT VALID` or deferrable constraints. The generated query's doc comment
shows the SQL sent and the rules applied.

| Rule | Rewrite |
|------|---------|
| `not_in` | `x NOT IN (SELECT y …)` → `NOT EXISTS (… WHERE x = y)` when neither side can be NULL: an anti-join, instead of a SubPlan that turns quadratic once the subquery outgrows `work_mem`. Only in the top-level `SELECT`'s `WHERE` (no `FOR UPDATE`/`SHARE`), over plain tables, next to conditions that can't fail, with a subquery that can't fail either: columns, constants, comparisons, and only the casts and functions that return a value for every input (`int4` to `numeric`, `lower`, `now() - interval '1 day'`; not `n::smallint`, `10 / n` or `text::int`); elsewhere (target lists, DML, nested queries) `NOT EXISTS` isn't a gain or isn't provably the same. |

They are on by default. Turn them all off, or one by one:

```toml
[package.metadata.typedpg]
simplify = false                   # none at all

# or, instead:
[package.metadata.typedpg.simplify]
not_in = false                     # every other rule stays on
```

The setting applies to every database. Queries with a `$..spread` are sent
as written.

For projects with more than one database see the [Multiple databases](#multiple-databases) section above.

## How it works

The `sql!` macro performs **fully static analysis** at compile time:

1. Reads your migration files from the configured path
2. Parses the DDL statements with PostgreSQL 19's own parser (its sources bundled via `typedpg_pg_query`)
3. Builds an in-memory schema snapshot by applying each migration's DDL on top of a built-in PostgreSQL 19 catalog seed
4. Parses your SQL query and resolves column types, parameter types, and nullability against the snapshot
5. Generates a concrete Rust struct with correctly typed fields

Everything runs in-process during `cargo build`: no database, no external process, fully reproducible.

Extensions are supported via built-in SQL definitions that the DDL interpreter processes automatically when it sees `CREATE EXTENSION`.

### Faster dev builds

Cargo compiles proc macros and their dependencies **unoptimized** in dev builds, so the analyzer behind `sql!` runs at debug speed — several times slower than it needs to. Opting it into optimization in your `Cargo.toml` makes each `sql!` and each migration replay much cheaper; the analyzer is then compiled optimized once and cached:

```toml
[profile.dev.package.typedpg_analyzer]
opt-level = 3

[profile.dev.package.typedpg_pg_query]
opt-level = 3
```

Measured on a schema of ~300 tables: replaying the migrations goes from ~0.5 s to ~0.2 s per crate build, and analyzing a query from ~0.3 ms to ~0.04 ms.

## Requirements

- Rust 1.89+

## License

MIT.
