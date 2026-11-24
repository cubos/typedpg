# Lint rules

`cargo typedpg lint` (Rust) and `npx typedpg lint` (TypeScript) check every
query against the schema your migrations build, and the migrations
themselves, without a database. Rules work from what the schema states
(types, nullability, `CHECK`s, unique and foreign keys, the locks a
statement takes), so they catch what a text-based linter can't, and stay
silent when they can't be sure.

Each rule links to a page with an example, what triggers it and what
doesn't. The level shown is the default, and its category's: a category key
sets every rule of the category.

## Queries

Rules over the SQL of each query (`sql!` in Rust, `sql` / `.sql` files in TypeScript).

| Rule | Category | Default | Reports |
|---|---|---|---|
| [`null_comparison`](null_comparison.md) | correctness | error | A comparison with a `NULL` literal, which is never true. |
| [`unbounded_write`](unbounded_write.md) | suspicious | warning | An `UPDATE` or `DELETE` without a `WHERE` clause. |
| [`not_in_nullable`](not_in_nullable.md) | correctness | error | `NOT IN (SELECT …)` that returns no rows because a side may be NULL. |
| [`redundant_null_check`](redundant_null_check.md) | redundant | info | `IS [NOT] NULL` or `COALESCE` on a value that is never NULL. |
| [`limit_without_order`](limit_without_order.md) | correctness | error | `LIMIT`, `OFFSET` or `DISTINCT ON` without `ORDER BY`, which picks arbitrary rows. |
| [`redundant_limit`](redundant_limit.md) | redundant | info | A `LIMIT` on a query that returns at most one row. |
| [`redundant_distinct`](redundant_distinct.md) | redundant | info | A `DISTINCT` on rows that are already distinct. |
| [`constant_condition`](constant_condition.md) | suspicious | warning | A condition that is always true or never true. |
| [`unreachable_case_branch`](unreachable_case_branch.md) | redundant | info | A `CASE` branch that is never taken. |
| [`case_without_else`](case_without_else.md) | style | info | A `CASE` without `ELSE` whose `WHEN`s may all be false, which makes it NULL. |
| [`redundant_cast`](redundant_cast.md) | redundant | info | A cast to the type its operand already has. |
| [`redundant_alias`](redundant_alias.md) | redundant | info | An output column alias that names the column what it already is. |
| [`outer_reference_in_subquery`](outer_reference_in_subquery.md) | suspicious | warning | An unqualified column in a subquery that resolves to an outer query. |
| [`cartesian_join`](cartesian_join.md) | suspicious | warning | `FROM` items joined with no condition connecting them. |
| [`insert_without_columns`](insert_without_columns.md) | suspicious | warning | An `INSERT` without a column list. |
| [`self_assignment`](self_assignment.md) | suspicious | warning | An `UPDATE` that assigns a column its own value. |
| [`unused_cte`](unused_cte.md) | redundant | info | A `WITH` query nothing references. |
| [`simplifiable_case`](simplifiable_case.md) | redundant | info | A `CASE` that is a `COALESCE` or its own condition. |
| [`like_unescaped_param`](like_unescaped_param.md) | correctness | error | A `LIKE` pattern built from a parameter whose `%` and `_` are not escaped. |
| [`redundant_join`](redundant_join.md) | redundant | info | A join that can't change the rows and whose table nothing else reads. |
| [`scalar_subquery_multiple_rows`](scalar_subquery_multiple_rows.md) | correctness | error | A scalar subquery used as a value that may return more than one row. |
| [`timestamp_timezone_mix`](timestamp_timezone_mix.md) | suspicious | warning | An implicit `timestamp` / `timestamptz` conversion that depends on the session's `TimeZone`. |
| [`integer_division`](integer_division.md) | suspicious | warning | An integer division whose truncated result is used as a non-integer value. |
| [`left_join_nullified`](left_join_nullified.md) | suspicious | warning | An outer join the `WHERE` clause turns into an inner join by rejecting its null-extended rows. |
| [`check_violation`](check_violation.md) | correctness | error | An `INSERT` or `UPDATE` storing a constant a `CHECK` constraint rejects. |
| [`unstable_order`](unstable_order.md) | correctness | error | An `ORDER BY` with `LIMIT`, `OFFSET` or `DISTINCT ON` whose keys can tie. |
| [`redundant_order_by`](redundant_order_by.md) | redundant | info | `ORDER BY` keys that can't change the order of the rows. |
| [`redundant_group_by`](redundant_group_by.md) | redundant | info | `GROUP BY` keys determined by the other keys or pinned to a constant. |
| [`redundant_count_argument`](redundant_count_argument.md) | redundant | info | `count(expr)` over a value that is never NULL, which is `count(*)`. |
| [`non_sargable_predicate`](non_sargable_predicate.md) | performance | warning | A comparison wrapping an indexed column so its index can't serve it. |
| [`float_equality`](float_equality.md) | suspicious | warning | An equality comparison of floating-point values. |
| [`comma_join`](comma_join.md) | style | info | A `FROM` list joining relations with commas instead of `JOIN … ON`. |
| [`ordinal_reference`](ordinal_reference.md) | style | info | `ORDER BY`, `GROUP BY` or `DISTINCT ON` by an output column's position. |
| [`boolean_comparison`](boolean_comparison.md) | style | info | A comparison of a boolean with the literal `true` or `false`. |

## `sql!` bindings

Rules over the Rust source of a `sql!` invocation, not its SQL. Rust only.

| Rule | Category | Default | Reports |
|---|---|---|---|
| [`redundant_binding`](redundant_binding.md) | redundant | info | A binding `x = x` (or a bare `x`) the parameter would capture without it. |
| [`renaming_binding`](renaming_binding.md) | redundant | info | A binding `p = v` that only renames a variable. |

## Call sites

Rules over how a query's result is used (`fetch_one`, `fetchOne`, `execute`, …). Findings are placed at the method call.

| Rule | Category | Default | Reports |
|---|---|---|---|
| [`fetch_one_multiple_rows`](fetch_one_multiple_rows.md) | correctness | error | `fetch_one` / `fetch_optional` / `fetch_value` / `fetch_value_optional` on a query not proven to return at most one row. |
| [`fetch_all_single_row`](fetch_all_single_row.md) | redundant | info | `fetch_all` on a query that returns at most one row. |
| [`unused_returning`](unused_returning.md) | redundant | info | `execute()` on a statement whose `RETURNING` rows are discarded. |
| [`json_stringify_param`](json_stringify_param.md) | correctness | error | TypeScript only: `JSON.stringify(…)` passed as a `json` / `jsonb` parameter's value, which stores a JSON string instead of the value. |

## Migrations

Rules over each migration statement. A table created by an earlier file is assumed to hold rows; one created earlier in the same file is assumed empty.

| Rule | Category | Default | Reports |
|---|---|---|---|
| [`create_index_blocking`](create_index_blocking.md) | locking | warning | `CREATE INDEX` (or an index-backed constraint) without `CONCURRENTLY` on a table that already has rows. |
| [`add_constraint_scan`](add_constraint_scan.md) | locking | warning | A `CHECK` or foreign key that validates existing rows under a lock. |
| [`set_not_null_scan`](set_not_null_scan.md) | locking | warning | `SET NOT NULL` that scans every row of the table. |
| [`alter_table_rewrite`](alter_table_rewrite.md) | locking | warning | `ALTER TABLE` that rewrites every row under a lock. |
| [`lock_without_timeout`](lock_without_timeout.md) | locking | warning | A migration taking a blocking lock on an existing table without `SET lock_timeout`. |
| [`drop_index_blocking`](drop_index_blocking.md) | locking | warning | `DROP INDEX` without `CONCURRENTLY` on a table that already has rows. |
| [`rebuild_blocking`](rebuild_blocking.md) | locking | warning | `REINDEX`, `VACUUM FULL`, `CLUSTER` or `REPACK` holding a blocking lock for the whole rebuild. |
| [`detach_partition_blocking`](detach_partition_blocking.md) | locking | warning | `DETACH PARTITION` without `CONCURRENTLY` on a table that already has rows. |
| [`validate_constraint_blocking`](validate_constraint_blocking.md) | locking | warning | `VALIDATE CONSTRAINT` in the transaction that added the constraint `NOT VALID`. |
| [`attach_partition_scan`](attach_partition_scan.md) | locking | warning | `ATTACH PARTITION` scanning the partition because its constraints don't imply its bound, or scanning the default partition. |
| [`deployed_query_breakage`](deployed_query_breakage.md) | correctness | error | With `--previous-deployment <ref>` only: a migration that breaks a query of the code deployed at that ref. |

## Final schema

Rules over the schema after the last migration. Each finding is placed on the statement that created the object, and a later migration that fixes the problem clears it.

| Rule | Category | Default | Reports |
|---|---|---|---|
| [`fk_without_index`](fk_without_index.md) | performance | warning | A foreign key whose referencing columns have no index. |
| [`redundant_index`](redundant_index.md) | performance | warning | An index another index on the table already covers. |
| [`fk_type_mismatch`](fk_type_mismatch.md) | performance | warning | A foreign key whose column types differ from the referenced columns. |
| [`table_without_primary_key`](table_without_primary_key.md) | suspicious | warning | A table without a primary key. |
| [`policy_without_rls`](policy_without_rls.md) | security | error | Row-level security policies on a table that does not enable row-level security. |
| [`rls_without_policy`](rls_without_policy.md) | security | error | Row-level security enabled on a table with no policy. |
| [`security_definer_without_search_path`](security_definer_without_search_path.md) | security | error | A `SECURITY DEFINER` routine without `SET search_path`. |
| [`prefer_timestamptz`](prefer_timestamptz.md) | pedantic | off | A `timestamp without time zone` column. |
| [`prefer_identity`](prefer_identity.md) | pedantic | off | A `serial` column instead of an identity column. |
| [`prefer_text`](prefer_text.md) | pedantic | off | A `varchar(n)` or `char(n)` column instead of `text`. |
| [`prefer_bigint_pk`](prefer_bigint_pk.md) | pedantic | off | An `int2` or `int4` primary key. |
| [`prefer_jsonb`](prefer_jsonb.md) | performance | warning | A `json` column instead of `jsonb`. |

## Not configurable

| Rule | Category | Level | Reports |
|---|---|---|---|
| [`invalid_allow`](invalid_allow.md) | — | error | A malformed or unknown rule name in a `typedpg-lint` comment. |
| [`internal_error`](internal_error.md) | — | warning | A typedpg bug: a fix that would have changed the query, withheld. |

## Configuring rules

### Running

```sh
cargo typedpg lint [--manifest-path ./Cargo.toml] [--format human|json|sarif] [--previous-deployment <ref>] [--fix]
npx typedpg lint [--config typedpg.config.json] [--format human|json|sarif] [--previous-deployment <ref>] [--fix]
```

Findings go to stdout, the summary to stderr. The run fails (non-zero exit)
when a finding reaches `fail_on`, or when a query or migration doesn't
analyze.

### Levels

Each rule is `"error"`, `"warning"`, `"info"` or `"off"`. Unlisted rules keep
the defaults above; give an `off` rule any level to enable it.

Every rule belongs to a category, and its default level is the category's:

| Category | Default |
|---|---|
| correctness, security | error |
| suspicious, locking, performance | warning |
| redundant, style | info |
| pedantic | off |

A category key sets every rule of the category; a rule's own key wins over
its category's. A `pedantic` rule enabled through its category gets that
level (`pedantic = "info"` turns every `prefer_*` rule on at info).

Rust: `[package.metadata.typedpg.lint]` in `Cargo.toml`, snake_case keys,
for every database of the crate:

```toml
[package.metadata.typedpg.lint]
fail_on = "warning"
style = "off"
correctness = "error"
null_comparison = "off"
prefer_timestamptz = "info"
ignore = ["tests/**"]
```

TypeScript: the top-level `lint` object of `typedpg.config.json`, camelCase
keys, for every database:

```json
{
  "lint": {
    "failOn": "warning",
    "style": "off",
    "correctness": "error",
    "nullComparison": "off",
    "preferTimestamptz": "info",
    "ignore": ["tests/**"]
  }
}
```

An unknown key or value is a configuration error and nothing is checked.
The `sql!` binding rules are Rust-only keys; `invalid_allow` and
`internal_error` can't be configured.

### `fail_on` / `failOn`

The lowest severity that fails the run: `"error"` (default), `"warning"` or
`"info"`. Lower findings are printed only.

### `ignore`

Globs relative to the directory of `Cargo.toml` or `typedpg.config.json`,
with `/`; `*` stays within a directory, `**` crosses them.

- Query files: findings are dropped. Queries are still analyzed, and one
  that doesn't compile still fails the run.
- Migration files: still applied to build the schema, but the migration
  rules skip them. Schema rules still report on their statements, with a
  help that puts the fix in a new migration.

### Allow comments

```sql
-- typedpg-lint: allow(unbounded_write)
DELETE FROM sessions;

/* typedpg-lint: allow(create_index_blocking, lock_without_timeout) */
CREATE INDEX users_email_idx ON users (email);
```

The text must be exactly `typedpg-lint: allow(` + comma-separated rule names
+ `)`, with nothing after it. Anything else starting with `typedpg-lint:`,
or an unknown rule name, is an [`invalid_allow`](invalid_allow.md) error.

A comment after a statement's `;`, on the line the statement ends on,
belongs to that statement, not the next one:

```sql
DELETE FROM sessions; -- typedpg-lint: allow(unbounded_write)
```

| Finding | Where the comment goes |
|---|---|
| query and call-site rules | anywhere in the query's SQL; covers the whole query |
| query and call-site rules, `.sql` file | inside the query, between its `/* @name */` comment and its statement, after its `;` on the statement's last line, or on the lines before its `/* @name */` comment (after the previous query's last line); comments after the file's last query are its |
| `sql!` binding rules | in the `sql!` template's SQL |
| migration rules | inside the statement, just before it, or after its `;` on the statement's last line; covers that statement only |
| schema rules | on the statement the finding is placed on (the `CREATE` of the object); covers everything it creates |
| [`deployed_query_breakage`](deployed_query_breakage.md) | on the breaking migration statement, or in the deployed query |

### `--fix`

Applies the fixes findings carry, writes the files, lints again and reports
what remains. A fix never changes a query's result columns (names, types,
nullability), its parameters or what it does; when that can't be
guaranteed, the fix isn't offered and the finding is reported without one.
A fix that fails typedpg's final check is withheld and reported as
[`internal_error`](internal_error.md). Migration findings have no fixes.

Rules with fixes: `redundant_null_check` (its `COALESCE` case),
`redundant_limit`, `redundant_distinct`, `constant_condition`,
`case_without_else`, `redundant_cast`, `redundant_alias`,
`insert_without_columns`, `self_assignment`, `unused_cte`,
`simplifiable_case`, `redundant_join`, `redundant_order_by`,
`redundant_group_by`, `redundant_count_argument`, `unused_returning`,
`ordinal_reference`, `boolean_comparison`, and in Rust `redundant_binding` and
`renaming_binding`.

### `--format`

- `human` (default): one box per finding, as on the rule pages, then a
  summary.
- `json`: one document with the findings, analysis failures and summary.
- `sarif`: SARIF 2.1.0 for code scanning, analysis failures included.

```yaml
- run: cargo typedpg lint --format sarif > lint.sarif
- uses: github/codeql-action/upload-sarif@v4
  if: always()
  with:
    sarif_file: lint.sarif
```

### `--previous-deployment <ref>`

The git ref of what runs in production now, e.g. `origin/main` or a release
tag (formerly `--ignore-migrations-from`, still accepted).

- Migration files identical at the ref have already run and can't change:
  the migration rules skip them. They still build the schema, schema rules
  still report on them (with the fix in a new migration), and their allow
  comments still apply. The summary counts them, e.g.
  `(300 unchanged from origin/main: statement rules skipped)`.
- [`deployed_query_breakage`](deployed_query_breakage.md) runs: the queries
  of the code at the ref are checked against the new schema, since that code
  keeps running while the migrations apply.

### What the migration rules assume

- Migrations apply in order. `extra_migrations` / `extraMigrations`
  directories build the schema too, but are never reported.
- A table from an earlier file holds rows; one created earlier in the same
  file is empty.
- A file runs in one transaction when `use_transaction` / `useTransaction`
  is on (default), unless its first line is `-- no-transaction`. This
  decides what is allowed (`CREATE INDEX CONCURRENTLY` fails in a
  transaction), how long locks are held, and whether `SET LOCAL` applies.
