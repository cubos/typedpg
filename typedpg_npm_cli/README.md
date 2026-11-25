# typedpg for TypeScript: the command and bundler plugins

The development package of
[`@cubos/typedpg`](https://www.npmjs.com/package/@cubos/typedpg), the
TypeScript runtime. It gives you the `typedpg` command, the bundler plugins
of `.sql` files, and the TypeScript 7.1 content mapper.

```sh
npm install @cubos/typedpg
npm install --save-dev @cubos/typedpg-cli
```

The command runs a native binary, installed for your platform as an optional
dependency (`@cubos/typedpg-cli-linux-x64-gnu`, `-darwin-arm64`, …), so a
production install of `@cubos/typedpg` never downloads it. Node.js 26 or
later.

## Commands

`typedpg <command>`; for the commands that read it, the config defaults to
`typedpg.config.json` in the current directory.

| Command | |
|---|---|
| `typedpg gen [--watch]` | Generate each database's module from the queries in the sources. `--watch` keeps running, regenerating as sources and migrations change. |
| `typedpg check` | Fail if a generated module is out of date, or a query has an error. |
| `typedpg lint` | Run the lint rules over every query and the database's migrations. Query rules: `null_comparison` (error) and `unbounded_write` (warning), comparisons with `NULL` and writes with no `WHERE`; `not_in_nullable` (error), a `NOT IN (SELECT …)` whose sides may be NULL; `redundant_null_check` (info), an `IS [NOT] NULL` or `COALESCE` on a never-NULL value; `limit_without_order` (error), a `LIMIT`, `OFFSET` or `DISTINCT ON` without `ORDER BY`; `redundant_limit` (info), a `LIMIT` on a query returning at most one row; `redundant_distinct` (info), a `DISTINCT` on already-distinct rows; `constant_condition` (warning), a `WHERE`, `ON`, `HAVING` or `FILTER` condition (or a part of one, or of a `CASE WHEN`) that is always or never true, folded by the planner or decided by the constraints and the conditions around it; `unreachable_case_branch` (info), a `CASE` branch never taken; `case_without_else` (info), a `CASE` without `ELSE` that may fall through to NULL; `redundant_cast` (info), a cast to the type the value already has; `redundant_alias` (info), an alias that is the column's own name; `outer_reference_in_subquery` (warning), an unqualified column in a subquery that resolves to the outer query; `cartesian_join` (warning), FROM items no condition connects; `insert_without_columns` (warning), an `INSERT` without a column list; `self_assignment` (warning), an `UPDATE … SET a = a`; `unused_cte` (info), a `WITH` query nothing references; `simplifiable_case` (info), a `CASE` that is a `COALESCE` or its own condition; `like_unescaped_param` (error), a `LIKE` pattern from a parameter whose `%` and `_` are not escaped; `redundant_join` (info), a join that can't change the rows and whose table nothing reads; `scalar_subquery_multiple_rows` (error), a subquery used as a value that may return more than one row; `timestamp_timezone_mix` (warning), an implicit `timestamp`/`date`–`timestamptz` conversion that depends on the session's `TimeZone`; `integer_division` (warning), a truncating integer division whose result is used as a `numeric` or floating-point value; `left_join_nullified` (warning), a `WHERE` condition rejecting the null-extended rows of an outer join, which then behaves as an inner join; `check_violation` (error), an `INSERT` or `UPDATE` storing a constant a `CHECK` constraint rejects; `unstable_order` (error), an `ORDER BY` under `LIMIT`, `OFFSET` or `DISTINCT ON` whose keys can tie; `redundant_order_by` (info), `ORDER BY` keys that can't change the order (after a unique prefix, pinned by `WHERE`, repeated, or in a one-row query or an `EXISTS` / `IN` subquery); `redundant_group_by` (info), `GROUP BY` keys the other keys determine or `WHERE` pins; `redundant_count_argument` (info), a `count(expr)` over a never-NULL value, which is `count(*)`; `non_sargable_predicate` (warning), a comparison wrapping an indexed column in a cast, function or arithmetic (or converting it implicitly) so its index can't serve it; `float_equality` (warning), an `=` / `<>` / `IN` comparison of floating-point values; `comma_join`, `ordinal_reference` and `boolean_comparison` (info), a FROM list joined with commas, a clause picking an output column by its position, and a boolean compared with the literal `true` or `false`; `fetch_one_multiple_rows` (error), `fetchOne`/`fetchValue`/`fetchOptional`/`fetchValueOptional` on a query not proven to return at most one row; `fetch_all_single_row` (info), `fetchAll` on a query that returns at most one row; `unused_returning` (info), `execute` on a statement whose `RETURNING` rows are discarded, reported at the call in the calling file; `json_stringify_param` (error), a `JSON.stringify(…)` passed as a `json` / `jsonb` value, which stores a JSON string, reported at the `JSON.stringify(…)` call. Migration rules: `create_index_blocking` (warning), `CREATE INDEX` without `CONCURRENTLY`; `add_constraint_scan` (warning), a constraint validated over existing rows; `set_not_null_scan` (warning), `SET NOT NULL` scanning the table; `alter_table_rewrite` (warning), an `ALTER TABLE` rewriting the table (a column change, `SET LOGGED`/`UNLOGGED`, `SET ACCESS METHOD`, or `SET TABLESPACE` copying it); `lock_without_timeout` (warning), a blocking lock without `SET lock_timeout`; `drop_index_blocking` (warning), `DROP INDEX` without `CONCURRENTLY`; `rebuild_blocking` (warning), `REINDEX`, `VACUUM FULL`, `CLUSTER` or `REPACK` holding a blocking lock for the whole rebuild; `detach_partition_blocking` (warning), `DETACH PARTITION` without `CONCURRENTLY`; `validate_constraint_blocking` (warning), a `VALIDATE CONSTRAINT` in the transaction that added the constraint `NOT VALID`; `attach_partition_scan` (warning), an `ATTACH PARTITION` scanning a partition with no `CHECK` on the key; `deployed_query_breakage` (error), a migration that breaks a query of the code at `--previous-deployment`. Schema rules: `fk_without_index` (warning), where a partial index over the referencing columns `WHERE col IS NOT NULL` counts; `redundant_index` (warning); `fk_type_mismatch` (warning), foreign-key column types that differ from the referenced ones; `table_without_primary_key` (warning), a table without a primary key; `policy_without_rls` (error), policies on a table without row-level security enabled; `rls_without_policy` (error), row-level security enabled with no policy; `security_definer_without_search_path` (error), a `SECURITY DEFINER` routine without `SET search_path`; `prefer_jsonb` (warning), a `json` column; and `prefer_timestamptz`, `prefer_identity`, `prefer_text` and `prefer_bigint_pk` (off), a timestamp without time zone, a serial column, a `varchar(n)`/`char(n)` column and an `int2`/`int4` primary key. Each rule's severity is configured in the `lint` object — by the rule's key, or by its category's (`correctness`, `security`, `suspicious`, `locking`, `performance`, `redundant`, `style`, `pedantic`), which sets every rule of the category; a rule's own key wins — whose `ignore` globs drop the findings of matching query files and make matching migration files schema-only (failures still fail), and a `-- typedpg-lint: allow(rule)` comment suppresses one in a query or migration statement. `--previous-deployment <ref>` (formerly `--ignore-migrations-from`) names the git ref deployed now: it skips the statement rules of migrations identical to it, while their schema rules still report the current schema's problems on the creating statements, with the fix in a new migration, and `deployed_query_breakage` analyzes the queries of the code at the ref against the new schema, reporting on the breaking migration statement each one that would fail or change for the code still running during the deploy. `--fix` writes the SQL fixes query findings carry into the sources and `.sql` files, then lints again to report what remains (migration findings never have fixes; a finding's edits apply all or none, overlaps are skipped, and a file that changed while linting is not fixed). `--format human|json|sarif` picks the report (human is the default; the JSON and SARIF documents include analysis failures). Exits non-zero when a finding reaches `lint.failOn` (default `"error"`) or a query or migration fails analysis. |
| `typedpg build` | Compile the `.sql` files `sql.include` names into the cache the loaders and bundler plugins read, so importing one runs no analysis. `--json` prints the outcome as JSON on stdout, which the loaders read. |
| `typedpg serve [--idle-timeout <seconds>]` | Serve TypeScript's content mapper and the loaders over JSON-RPC on stdin/stdout. `--idle-timeout` makes it exit after that many seconds without a request; by default it does not exit. |
| `typedpg migrate up` | Apply all pending migrations. |
| `typedpg migrate status` | Show each migration's status. |
| `typedpg migrate down [name] [--force]` | Revert a migration (the last applied one by default). `--force` reverts even without a `.down.sql` file. |
| `typedpg migrate accept <name>` | Record an applied migration's edited file as applied, without running it. |
| `typedpg migrate create <name>` | Create an empty `<timestamp>_<name>.sql` and `<timestamp>_<name>.down.sql`. It needs no database. |

`gen`, `check`, `lint` and `build` take `--config <path>`. `lint` also takes
`--previous-deployment <ref>` and `--fix`. `migrate` takes
`--config`, `--db <name>` (when the config has several databases) and
`--url <url>` (the connection URL, defaulting to `DATABASE_URL`; a `.env`
file is read, and a variable already set in the environment wins over it). The migrate actions other than `create` need a connection.

In CI, `npx typedpg check` exits non-zero when a generated module is stale,
when a query has an error, or when a module remains for a `.sql` file that is
gone. `npx typedpg lint` exits non-zero when a finding reaches its configured
`failOn` severity, or when a query or migration fails to apply or analyze.

## Bundler plugins

Each plugin loads the module of an imported `.sql` file from the cache
`typedpg build` fills.

| Bundler | |
|---|---|
| esbuild | `import typedpg from "@cubos/typedpg-cli/esbuild"`, then `plugins: [typedpg()]` |
| Vite, vitest, Rollup, Rolldown | `import typedpg from "@cubos/typedpg-cli/vite"` (or `/rollup`), then `plugins: [typedpg()]` |
| webpack, rspack | `{ test: /\.sql$/, loader: "@cubos/typedpg-cli/webpack", type: "javascript/esm" }` |

On a cache miss, the plugins compile asynchronously through a persistent
`typedpg serve`, one child per binary: it keeps the project's schema in
memory between compilations and exits after five minutes without a request
(`TYPEDPG_SERVE_IDLE_TIMEOUT`, in seconds); the next cache miss spawns
another. The Node hook (`@cubos/typedpg/register`) is synchronous and runs
`typedpg build` once instead. A cache hit never needs a binary.

## TypeScript 7.1 content mapper

TypeScript 7.1 gets a `.sql` file's types from this package: it runs
`typedpg serve` as the file's content mapper, so no module has to be
written (`emit: "none"`). The CLI must be a devDependency:

```json
{
  "contentMappers": [{ "package": "@cubos/typedpg-cli", "extensions": [".sql"] }]
}
```

`tsc --runExternalCode` then checks the `.sql` files too, each error at its
place in the file. See
[The content mapper](https://github.com/cubos/typedpg/blob/main/typedpg_npm/README.md#the-content-mapper-typescript-71)
in the runtime's README.

## Environment variables

| Variable | |
|---|---|
| `DATABASE_URL` | The connection URL for `typedpg migrate`, when `--url` is not given. A `.env` file is read; a variable already set in the environment wins over it. |
| `TYPEDPG_BINARY` | Run this binary instead of the platform package. |
| `TYPEDPG_CACHE_DIR` | The `.sql` cache directory. Default: `node_modules/.cache/typedpg` beside the config file. |
| `TYPEDPG_SERVE_IDLE_TIMEOUT` | Seconds a `typedpg serve` spawned by the loaders or plugins lives without a request. Default: 300. |

## Platforms

| OS | Architectures |
|---|---|
| Linux (glibc) | `x64`, `arm64` |
| Linux (musl) | `x64`, `arm64` |
| macOS | `x64`, `arm64` |
| Windows | `x64` (`win32-x64-msvc`) |

Each is a package of its own (`@cubos/typedpg-cli-linux-x64-gnu`,
`@cubos/typedpg-cli-linux-arm64-musl`, `@cubos/typedpg-cli-darwin-x64`,
`@cubos/typedpg-cli-darwin-arm64`, `@cubos/typedpg-cli-win32-x64-msvc`, …),
installed as an optional dependency of `@cubos/typedpg-cli`. An install that
skips optional dependencies (`--omit=optional`, `--no-optional`) has no
binary: reinstall with them, or set `TYPEDPG_BINARY`.

On another platform, build one from source and point `TYPEDPG_BINARY` at
it:

```sh
cargo build --release -p typedpg_ts --bin typedpg
```

## License

MIT.
