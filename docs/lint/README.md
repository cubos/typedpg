# Lint rules

`cargo typedpg lint` (Rust) and `npx typedpg lint` (TypeScript) analyze every query against the schema your migrations build, without a database, and lint the migrations themselves. Because each rule works from what the analyzer proves — column types, nullability, `CHECK` constraints, unique keys, foreign keys, the locks a statement takes — it can report what a text-based linter can't, and stay silent when a fact can't be proven.

Each rule below links to a page with its motivation, exact conditions, verified examples and configuration. The level is the default; "off" rules report at `info` once you give them a level.

## Queries

Rules over the SQL of each query (`sql!` in Rust, `sql` / `.sql` files in TypeScript).

| Rule | Default | Reports |
|---|---|---|
| [`null_comparison`](null_comparison.md) | error | A comparison with a `NULL` literal, which is never true. |
| [`unbounded_write`](unbounded_write.md) | error | An `UPDATE` or `DELETE` without a `WHERE` clause. |
| [`not_in_nullable`](not_in_nullable.md) | warning | `NOT IN (SELECT …)` that returns no rows because a side may be NULL. |
| [`redundant_null_check`](redundant_null_check.md) | warning | `IS [NOT] NULL` or `COALESCE` on a value that is never NULL. |
| [`limit_without_order`](limit_without_order.md) | warning | `LIMIT`, `OFFSET` or `DISTINCT ON` without `ORDER BY`, which picks arbitrary rows. |
| [`redundant_limit`](redundant_limit.md) | info | A `LIMIT` on a query that returns at most one row. |
| [`redundant_distinct`](redundant_distinct.md) | info | A `DISTINCT` on rows that are already distinct. |
| [`constant_condition`](constant_condition.md) | warning | A condition that is always true or never true. |
| [`unreachable_case_branch`](unreachable_case_branch.md) | warning | A `CASE` branch that is never taken. |
| [`case_without_else`](case_without_else.md) | warning | A `CASE` without `ELSE` whose `WHEN`s may all be false, which makes it NULL. |
| [`redundant_cast`](redundant_cast.md) | info | A cast to the type its operand already has. |
| [`redundant_alias`](redundant_alias.md) | info | An output column alias that names the column what it already is. |
| [`outer_reference_in_subquery`](outer_reference_in_subquery.md) | warning | An unqualified column in a subquery that resolves to an outer query. |
| [`cartesian_join`](cartesian_join.md) | warning | `FROM` items joined with no condition connecting them. |
| [`insert_without_columns`](insert_without_columns.md) | warning | An `INSERT` without a column list. |
| [`self_assignment`](self_assignment.md) | warning | An `UPDATE` that assigns a column its own value. |
| [`unused_cte`](unused_cte.md) | warning | A `WITH` query nothing references. |
| [`simplifiable_case`](simplifiable_case.md) | info | A `CASE` that is a `COALESCE` or its own condition. |
| [`like_unescaped_param`](like_unescaped_param.md) | off | A `LIKE` pattern built from a parameter whose `%` and `_` are not escaped. |
| [`redundant_join`](redundant_join.md) | info | A join that can't change the rows and whose table nothing else reads. |
| [`scalar_subquery_may_return_many`](scalar_subquery_may_return_many.md) | warning | A scalar subquery used as a value that may return more than one row. |
| [`timestamp_timezone_mix`](timestamp_timezone_mix.md) | warning | An implicit `timestamp` / `timestamptz` conversion that depends on the session's `TimeZone`. |
| [`integer_division`](integer_division.md) | warning | An integer division whose truncated result is used as a non-integer value. |
| [`left_join_nullified`](left_join_nullified.md) | warning | An outer join the `WHERE` clause turns into an inner join by rejecting its null-extended rows. |
| [`check_violation`](check_violation.md) | warning | An `INSERT` or `UPDATE` storing a constant a `CHECK` constraint rejects. |
| [`unstable_order`](unstable_order.md) | warning | An `ORDER BY` with `LIMIT`, `OFFSET` or `DISTINCT ON` whose keys can tie. |
| [`redundant_order_by`](redundant_order_by.md) | info | `ORDER BY` keys that can't change the order of the rows. |
| [`redundant_group_by`](redundant_group_by.md) | info | `GROUP BY` keys determined by the other keys or pinned to a constant. |
| [`count_not_null`](count_not_null.md) | info | `count(expr)` over a value that is never NULL, which is `count(*)`. |
| [`non_sargable_predicate`](non_sargable_predicate.md) | info | A comparison wrapping an indexed column so its index can't serve it. |
| [`float_equality`](float_equality.md) | warning | An equality comparison of floating-point values. |

## `sql!` bindings

Rules over the Rust source of a `sql!` invocation, not its SQL. Only `cargo typedpg lint` runs them, and only the Rust configuration accepts their keys.

| Rule | Default | Reports |
|---|---|---|
| [`redundant_binding`](redundant_binding.md) | warning | A binding `x = x` (or a bare `x`) the parameter would capture without it. |
| [`renaming_binding`](renaming_binding.md) | info | A binding `p = v` that only renames a variable. |

## Call sites

Rules over how a query's result is used — `fetch_one`, `fetchOne`, `execute`, … — against what the schema proves about its rows. Both CLIs run them; findings are placed at the method call.

| Rule | Default | Reports |
|---|---|---|
| [`fetch_may_return_many`](fetch_may_return_many.md) | warning | `fetch_one` / `fetch_optional` on a query not proven to return at most one row. |
| [`fetch_all_single_row`](fetch_all_single_row.md) | info | `fetch_all` on a query that returns at most one row. |
| [`unused_returning`](unused_returning.md) | info | `execute()` on a statement whose `RETURNING` rows are discarded. |

## Migrations

Rules over each migration statement, on tables that already existed when its file started — typedpg can't see row counts, so it treats every table an earlier file created as one that holds rows, and every table created earlier in the same file as empty.

| Rule | Default | Reports |
|---|---|---|
| [`create_index_blocking`](create_index_blocking.md) | warning | `CREATE INDEX` (or an index-backed constraint) without `CONCURRENTLY` on a table that already has rows. |
| [`constraint_validating`](constraint_validating.md) | warning | A `CHECK` or foreign key that validates existing rows under a lock. |
| [`set_not_null_scan`](set_not_null_scan.md) | warning | `SET NOT NULL` that scans every row of the table. |
| [`table_rewrite`](table_rewrite.md) | warning | `ALTER TABLE` that rewrites every row under a lock. |
| [`missing_lock_timeout`](missing_lock_timeout.md) | warning | A migration taking a blocking lock on an existing table without `SET lock_timeout`. |
| [`drop_index_blocking`](drop_index_blocking.md) | warning | `DROP INDEX` without `CONCURRENTLY` on a table that already has rows. |
| [`blocking_rebuild`](blocking_rebuild.md) | warning | `REINDEX`, `VACUUM FULL`, `CLUSTER` or `REPACK` holding a blocking lock for the whole rebuild. |
| [`detach_partition_blocking`](detach_partition_blocking.md) | warning | `DETACH PARTITION` without `CONCURRENTLY` on a table that already has rows. |
| [`validate_in_same_transaction`](validate_in_same_transaction.md) | warning | `VALIDATE CONSTRAINT` in the transaction that added the constraint `NOT VALID`. |
| [`attach_partition_scan`](attach_partition_scan.md) | warning | `ATTACH PARTITION` scanning the partition because no `CHECK` implies its bound. |
| [`breaks_deployed_queries`](breaks_deployed_queries.md) | warning | With `--previous-deployment <ref>` only: a migration that breaks a query of the code deployed at that ref. |

## Final schema

Rules over the schema the migrations leave behind, run once after the last migration. Each finding is placed on the statement that created the object (the column's type, the constraint, the index, the policy, the routine), so a later migration that fixes the problem makes it go away.

| Rule | Default | Reports |
|---|---|---|
| [`fk_without_index`](fk_without_index.md) | warning | A foreign key whose referencing columns have no index. |
| [`redundant_index`](redundant_index.md) | info | An index another index on the table already covers. |
| [`fk_type_mismatch`](fk_type_mismatch.md) | warning | A foreign key whose column types differ from the referenced columns. |
| [`missing_primary_key`](missing_primary_key.md) | warning | A table without a primary key. |
| [`rls_policy_not_enforced`](rls_policy_not_enforced.md) | error | Row-level security policies on a table that does not enable row-level security. |
| [`rls_no_policy`](rls_no_policy.md) | info | Row-level security enabled on a table with no policy. |
| [`security_definer_search_path`](security_definer_search_path.md) | warning | A `SECURITY DEFINER` routine without `SET search_path`. |
| [`prefer_timestamptz`](prefer_timestamptz.md) | off | A `timestamp without time zone` column. |
| [`prefer_identity`](prefer_identity.md) | off | A `serial` column instead of an identity column. |
| [`prefer_text`](prefer_text.md) | off | A `varchar(n)` or `char(n)` column instead of `text`. |
| [`prefer_bigint_pk`](prefer_bigint_pk.md) | off | An `int2` or `int4` primary key. |
| [`prefer_jsonb`](prefer_jsonb.md) | off | A `json` column instead of `jsonb`. |

## Not configurable

| Rule | Level | Reports |
|---|---|---|
| [`invalid_allow`](invalid_allow.md) | error | A malformed or unknown rule name in a `typedpg-lint` comment. |
| [`internal_error`](internal_error.md) | warning | A typedpg bug: a fix that would have changed the query, withheld. |

## Configuring rules

### Running

```sh
cargo typedpg lint [--manifest-path ./Cargo.toml] [--format human|json|sarif] [--previous-deployment <ref>] [--fix]
npx typedpg lint [--config typedpg.config.json] [--format human|json|sarif] [--previous-deployment <ref>] [--fix]
```

Findings go to stdout and the summary to stderr. The run exits non-zero when a finding reaches the `fail_on` severity, or when a query or migration fails analysis.

### Levels

Every rule has a level: `"error"`, `"warning"`, `"info"` or `"off"`. Rules you don't mention keep their default (the tables above). `like_unescaped_param` and the `prefer_*` rules are off by default; give them any level to enable them.

In Rust, the levels go in `Cargo.toml`, under `[package.metadata.typedpg.lint]`, with the rule names as keys. The section applies to every database of the crate (`[package.metadata.typedpg.databases.*]` included):

```toml
[package.metadata.typedpg.lint]
fail_on = "warning"          # the severity that fails the run: "error" (default), "warning" or "info"
null_comparison = "off"
redundant_index = "warning"
prefer_timestamptz = "info"
ignore = ["tests/**"]        # see below
```

In TypeScript, they go in `typedpg.config.json`'s top-level `lint` object, for every database, with the rule names in camelCase:

```json
{
  "include": ["src"],
  "out": "src/db.ts",
  "migrations": "migrations",
  "lint": {
    "failOn": "warning",
    "nullComparison": "off",
    "redundantIndex": "warning",
    "preferTimestamptz": "info",
    "ignore": ["tests/**"]
  }
}
```

An unknown key or an invalid value is a configuration error. `redundant_binding` and `renaming_binding` are only accepted in the Rust configuration; `invalid_allow` and `internal_error` are accepted in neither.

### `fail_on` / `failOn`

The severity at which a finding fails the run: `"error"` (the default), `"warning"` or `"info"`. With `"warning"`, every warning or error fails it. `invalid_allow` is always an error, so it always fails the run. Info findings are printed but only fail the run with `"info"`.

### `ignore`

Glob patterns, relative to the directory of `Cargo.toml` (Rust) or of `typedpg.config.json` (TypeScript), with forward slashes; `*` does not cross a `/`, `**` does.

- Findings in a matching query file are dropped. Its queries are still analyzed, and one that does not compile still fails the run.
- A matching migration file is still applied and builds the schema, but its statement rules (the migration rules above) are skipped. Its schema findings are still reported, on the statements that created the objects, with a help line that puts the fix in a new migration, and its allow comments are still honored and validated.

### Allow comments

A comment suppresses rules for one query or one migration statement:

```sql
-- typedpg-lint: allow(unbounded_write)
DELETE FROM sessions;

/* typedpg-lint: allow(create_index_blocking, missing_lock_timeout) */
CREATE INDEX users_email_idx ON users (email);
```

The comment must be exactly `typedpg-lint: allow(` followed by comma-separated rule names and `)`, with nothing after it; anything else starting with `typedpg-lint:` is an [`invalid_allow`](invalid_allow.md) error, as is an unknown rule name.

- **Queries.** The comment can be anywhere in the query's SQL and covers the whole query, including the call-site rules' findings for it. In Rust, the binding rules read the allow comments of the `sql!` template's SQL.
- **Migration statements.** The comment covers the statement it is inside, or the statement it leads into (a comment between the previous statement and this one) — not the rest of the file.
- **Schema rules.** The finding is suppressed by an allow comment on the statement it is placed on: the one that created the column, constraint, index, table, policy or routine (or enabled row-level security). The comment covers every object that statement creates.
- **`breaks_deployed_queries`.** On the migration statement that breaks the query, or in the deployed query's own SQL.

### `--fix`

`--fix` applies the fixes findings carry, writes the files, then lints again and reports what remains. In Rust it deletes `redundant_binding` bindings, rewrites `renaming_binding` parameters, and applies SQL edits; in TypeScript it writes SQL edits into the sources and `.sql` files. The SQL fixes come from `redundant_null_check` (its `COALESCE`), `redundant_limit`, `redundant_distinct`, `constant_condition`, `case_without_else`, `redundant_cast`, `redundant_alias`, `insert_without_columns`, `self_assignment`, `unused_cte`, `simplifiable_case`, `redundant_join`, `redundant_order_by`, `redundant_group_by`, `count_not_null` and `unused_returning`. Migration findings never have fixes.

A fix is only offered when its rule proves it keeps the query's meaning, and every fix is checked again before it is offered: the edited query must analyze to the same output columns (names, types, nullability, known values), the same parameters and the same ability to run as a subquery (`unused_returning`'s must have no output columns). A fix that fails that check is withheld and reported as [`internal_error`](internal_error.md).

### `--format`

- `human` (the default): one box per finding, as in the rule pages, then a summary.
- `json`: one JSON document with the findings, the analysis failures and the summary.
- `sarif`: SARIF 2.1.0, for code scanning (each rule's one-line description is its SARIF metadata). Both `json` and `sarif` include the analysis failures, which the human format prints on stderr.

```yaml
- run: cargo typedpg lint --format sarif > lint.sarif
- uses: github/codeql-action/upload-sarif@v4
  if: always()
  with:
    sarif_file: lint.sarif
```

### `--previous-deployment <ref>`

Names the git ref of what is deployed now, for example `origin/main` or a release tag. Its former name, `--ignore-migrations-from`, still works. It does two things:

- **Applied migrations skip the statement rules.** A migration file identical to its version at the ref has already run in production and can't be changed, so its locks, scans and rewrites are no longer reported. It is still applied for the schema, the schema rules still report on its statements (with help lines that put the fix in a new migration), and its allow comments are still honored and validated. A file absent from the ref, or changed by the branch, is linted as usual. The summary says how many files this applied to, e.g. `(300 unchanged from origin/main: statement rules skipped)`.
- **[`breaks_deployed_queries`](breaks_deployed_queries.md) runs.** The code at the ref keeps running while the new migrations apply and during the rollout; its queries, read from the ref, are analyzed against the new schema, and each one that breaks is reported on the migration statement that breaks it.

### What the migration rules assume

- Migration files are applied in order to build the schema. `extra_migrations` (Rust) / `extraMigrations` (TypeScript) directories build it too, but nothing in them is ever reported: they are another project's schema.
- A table that existed before a file started is assumed to hold rows; one created earlier in the same file is assumed empty.
- Whether a file runs in a transaction follows the runner: `use_transaction` / `useTransaction` (default true), unless the file's first line is `-- no-transaction`. This decides which statements are allowed at all (`CREATE INDEX CONCURRENTLY` inside a transaction is an error, as in PostgreSQL), how long a lock is held, and whether `SET LOCAL` has any effect.
