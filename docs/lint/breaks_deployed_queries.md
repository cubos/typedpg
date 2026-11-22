# `breaks_deployed_queries`

A new migration breaks a query of the code that is deployed right now — the code that keeps running while the migration is applied and until the new code has rolled out.

| | |
|---|---|
| Default level | warning |
| Applies to | migrations (with `--previous-deployment <ref>` only) |
| Fix | no |

## Why

A deploy is not atomic. The usual order is:

1. The new migrations run against the production database.
2. The new code rolls out, instance by instance.

During step 1, and for all of step 2, the **previous** code is still serving traffic — against the **new** schema. Your branch's queries were type-checked against the new schema, so they are fine; the queries that break are the old ones. Rename a column and every old instance fails with `column "email" does not exist` until it is replaced. Add an enum label and old code meets a value it cannot decode. Make a column nullable and old code that was generated with a non-null type receives a NULL. Add a `NOT NULL` column without a default and every old `INSERT` fails.

typedpg can see this before the deploy because it analyzes queries against a schema without a database: it builds the schema of the deployed code from the migrations at the deployed git ref, takes the deployed code's queries from that same ref, and analyzes each of them a second time against the schema your branch's migrations build.

The fix is the expand/contract pattern: never change the schema under a running query. First add what the new code needs (expand) and deploy code that works with both shapes; remove what the old code needed (contract) only in a later deploy, when no running code uses it anymore.

## What it reports

It runs only when you pass `--previous-deployment <ref>` (for example `origin/main`, or the tag of the release in production). The flag was called `--ignore-migrations-from`; that name still works.

```sh
cargo typedpg lint --previous-deployment origin/main
npx typedpg lint --previous-deployment origin/main
```

typedpg checks out the ref into a temporary snapshot and reads the project there exactly as it reads it now: its configuration, its migrations (which build the deployed schema, per database) and its queries (`sql!` invocations, or the TypeScript sources and `.sql` files). Queries come from the ref, so a query your branch changed or deleted is still checked — the deployed code still runs it. A migration file identical to its version at the ref is an applied migration; the first file that is absent from the ref or differs from it is the first new one. With no new migration, nothing is reported.

Each deployed query that analyzes against the deployed schema is analyzed again against the new one. It is reported when, against the new schema:

- it no longer analyzes — a dropped or renamed table or column, a type an operator no longer accepts. The message quotes PostgreSQL's error.
- a result column is gone, changes type (type modifiers and collations aside), or may now be NULL;
- the list of result columns changes (a `SELECT *` gaining a column): the generated code checks the columns it receives;
- a result value may now be one the deployed code doesn't know: a new enum label, or a `CHECK` that admits more values or a wider range (also for array elements and the fields of a composite or record);
- the elements of an array result may now be NULL, or a field of a record result may now be NULL;
- a parameter (or a field of a `$..spread`) changes type, no longer accepts NULL, or no longer accepts an enum label it used to;
- an `INSERT` leaves out a column that the new schema makes `NOT NULL` without a default, identity or generation expression, where the old schema let it be left out. (With `VALUES`, the analysis itself already fails with PostgreSQL's not-null error; with `INSERT … SELECT` this check reports it.)

A query that does not analyze against its own deployed schema was already broken there, and is skipped.

### Where the finding goes

The new migrations are replayed one statement at a time, and each change is placed on the first statement after which it shows: the `DROP COLUMN`, the `ALTER TYPE … ADD VALUE`. One finding is reported per deployed query and statement; several changes that one statement causes to one query are joined with `;`. The message names the query by its file and line at the ref (and every other place the same SQL text appears). A change that only an `extra_migrations` file causes, or that the replay could not place, goes to the first line of the first new migration.

## Examples

### Bad

The deployed code (commit `HEAD`) has this migration and these queries:

```sql
-- migrations/0001_init.sql
CREATE TYPE user_status AS ENUM ('active', 'blocked');
CREATE TABLE users (
    id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    email text NOT NULL,
    name text NOT NULL,
    status user_status NOT NULL DEFAULT 'active'
);
```

```rust
// src/lib.rs at HEAD
sql!(pool, "SELECT email, name FROM users WHERE id = $id").fetch_one().await?;  // line 4
sql!(pool, "SELECT id, status FROM users").fetch_all().await?;                 // line 5
```

The branch renames `email` (and updates its own query to `email_address`) and adds an enum label:

```sql
-- migrations/0002_users.sql
ALTER TABLE users RENAME COLUMN email TO email_address;
ALTER TYPE user_status ADD VALUE 'deleted';
```

`cargo typedpg lint --previous-deployment HEAD`:

```text
warning[breaks_deployed_queries]: this migration breaks the deployed query at src/lib.rs:4 (HEAD): it no longer runs: column "email" does not exist
   ╭─[migrations/0002_users.sql:1:1]
 1 │ ALTER TABLE users RENAME COLUMN email TO email_address;
   · ──────────────────────────────────────────────────────
   ╰────
  help: the code deployed at HEAD keeps running while the migration is applied and during the rollout: first deploy code that works with both schemas, then the migration (expand/contract)

warning[breaks_deployed_queries]: this migration breaks the deployed query at src/lib.rs:5 (HEAD): result column "status" may now be 'deleted', a value the deployed code does not know
   ╭─[migrations/0002_users.sql:2:1]
 2 │ ALTER TYPE user_status ADD VALUE 'deleted';
   · ──────────────────────────────────────────
   ╰────
  help: the code deployed at HEAD keeps running while the migration is applied and during the rollout: first deploy code that works with both schemas, then the migration (expand/contract)
```

Other changes it reports, from deployed queries `SELECT * FROM users` (line 4), `SELECT id, name FROM users` (line 6) and `INSERT INTO users (email, name) SELECT email, 'copy' FROM users WHERE id = 1` (line 7):

```sql
-- migrations/0002_change.sql
SET LOCAL lock_timeout = '5s';
ALTER TABLE users ADD COLUMN tenant_id bigint NOT NULL;
ALTER TABLE users ALTER COLUMN name DROP NOT NULL;
```

```text
warning[breaks_deployed_queries]: this migration breaks the deployed query at src/lib.rs:4 (HEAD): its result columns become (id, email, name, status, tenant_id) instead of (id, email, name, status), and the deployed code checks them
   ╭─[migrations/0002_change.sql:2:1]
 2 │ ALTER TABLE users ADD COLUMN tenant_id bigint NOT NULL;
   · ──────────────────────────────────────────────────────
   ╰────
  help: the code deployed at HEAD keeps running while the migration is applied and during the rollout: first deploy code that works with both schemas, then the migration (expand/contract)

warning[breaks_deployed_queries]: this migration breaks the deployed query at src/lib.rs:7 (HEAD): its INSERT into users leaves out column tenant_id, now NOT NULL without a default, so inserting any row fails
   ╭─[migrations/0002_change.sql:2:1]
 2 │ ALTER TABLE users ADD COLUMN tenant_id bigint NOT NULL;
   · ──────────────────────────────────────────────────────
   ╰────
  help: the code deployed at HEAD keeps running while the migration is applied and during the rollout: first deploy code that works with both schemas, then the migration (expand/contract)

warning[breaks_deployed_queries]: this migration breaks the deployed query at src/lib.rs:4 (HEAD): result column "name" may now be NULL
   ╭─[migrations/0002_change.sql:3:1]
 3 │ ALTER TABLE users ALTER COLUMN name DROP NOT NULL;
   · ─────────────────────────────────────────────────
   ╰────
  help: the code deployed at HEAD keeps running while the migration is applied and during the rollout: first deploy code that works with both schemas, then the migration (expand/contract)

warning[breaks_deployed_queries]: this migration breaks the deployed query at src/lib.rs:6 (HEAD): result column "name" may now be NULL
   ╭─[migrations/0002_change.sql:3:1]
 3 │ ALTER TABLE users ALTER COLUMN name DROP NOT NULL;
   · ─────────────────────────────────────────────────
   ╰────
  help: the code deployed at HEAD keeps running while the migration is applied and during the rollout: first deploy code that works with both schemas, then the migration (expand/contract)
```

A deployed `UPDATE users SET name = $name WHERE id = $id` is not reported by `DROP NOT NULL`: its parameter accepts more than before, not less.

### Good

Rename `email` to `email_address` with expand/contract, over three deploys.

Deploy 1 — expand. Add the new column; the code deployed now still reads `email`, names the columns it selects, and nothing it runs changes (a deployed `SELECT *` would be reported: its column list grows):

```sql
-- migrations/0002_email_address.sql
SET LOCAL lock_timeout = '5s';
ALTER TABLE users ADD COLUMN email_address text;
-- A NOT NULL column with a default doesn't break the deployed INSERTs.
ALTER TABLE users ADD COLUMN tenant_id bigint NOT NULL DEFAULT 1;
CREATE TABLE tenants (id bigint PRIMARY KEY);
```

Deploy 2 — code that writes both columns and reads `email_address` (backfilling the old rows in between).

Deploy 3 — contract. With `--previous-deployment` pointing at deploy 2, whose queries only use `email_address`, dropping `email` breaks nothing and is not reported:

```sql
-- migrations/0003_drop_email.sql
SET LOCAL lock_timeout = '5s';
ALTER TABLE users DROP COLUMN email;
```

Run the same migration while the deployed code still reads `email`, and it is reported at once:

```text
warning[breaks_deployed_queries]: this migration breaks the deployed query at src/lib.rs:4 (HEAD): it no longer runs: column "email" does not exist
```

## When it stays silent

- Without `--previous-deployment`, or when every migration file is identical to the ref.
- A deployed query that does not analyze against the deployed schema.
- A change that does not affect what the deployed code sends or receives: a new nullable column it does not select, a new table, an index, a parameter that now accepts more values.
- An allow comment on the migration statement, or in the deployed query's SQL (see below).
- A ref where the project has no typedpg configuration, or a database whose migrations don't build at the ref: there is nothing deployed to compare with.

The rule sees the queries typedpg analyzes. SQL built at run time, queries in other services and ad-hoc scripts are outside its view.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
breaks_deployed_queries = "error"   # "error", "warning" (default), "info" or "off"
```

```json
// typedpg.config.json
{ "lint": { "breaksDeployedQueries": "error" } }
```

With `"error"` and the default `fail_on = "error"`, a breaking migration fails the run. A typical CI job compares a branch with what production runs:

```sh
git fetch origin main
cargo typedpg lint --previous-deployment origin/main
```

To accept a break — for example, when the code that used the column is already gone everywhere — put the allow comment on the migration statement:

```sql
SET LOCAL lock_timeout = '5s';
-- typedpg-lint: allow(breaks_deployed_queries)
ALTER TABLE users DROP COLUMN email;
```

or in the deployed query's SQL, at the ref (`-- typedpg-lint: allow(breaks_deployed_queries)` inside the query text).

The same flag also makes migration files identical to the ref skip the statement rules (their locks and scans already happened in production) while the schema rules still report on them: see [Configuring rules](README.md#configuring-rules).

## Related rules

- [`table_rewrite`](table_rewrite.md): the contract step of a type change.
- [`missing_lock_timeout`](missing_lock_timeout.md): the other risk of the same `ALTER TABLE`.
- The schema rules ([`fk_without_index`](fk_without_index.md) and the rest), whose help lines change under `--previous-deployment`.
