# `create_index_blocking`

A migration builds an index on an existing table without `CONCURRENTLY`, blocking every write to the table until the build finishes.

| | |
|---|---|
| Default level | warning |
| Applies to | migrations |
| Fix | no |

## Why

A plain `CREATE INDEX` takes a `SHARE` lock on the table. `SELECT` keeps working, but every `INSERT`, `UPDATE`, `DELETE` and `MERGE` waits for the whole build: on a large table that can take minutes, and your application's writes queue up for all of it. When the migration runs in a transaction, which is the default, the lock is held until the transaction commits, so it also covers whatever else the file does after the index.

`CREATE INDEX CONCURRENTLY` takes a `SHARE UPDATE EXCLUSIVE` lock instead, which does not conflict with reads or writes. It scans the table twice and waits for running transactions, so it is slower, and PostgreSQL refuses to run it inside a transaction block. A typedpg migration whose first line is `-- no-transaction` runs without the runner's transaction.

Adding a `PRIMARY KEY`, `UNIQUE` or `EXCLUDE` constraint builds the same kind of index, under an even stronger lock (`ALTER TABLE ADD CONSTRAINT` takes `ACCESS EXCLUSIVE`, which blocks reads too: PostgreSQL's `AlterTableGetLockLevel`). The rule reports that case as well, and its help suggests building the unique index concurrently first and adopting it with `ADD CONSTRAINT … UNIQUE / PRIMARY KEY USING INDEX`. An `EXCLUDE` constraint cannot adopt an index, so for it the help only says to add it while the table is small or can be locked for the whole build.

## What it reports

- `CREATE INDEX` without `CONCURRENTLY` on a table, partitioned table or foreign table that existed before the current migration file started.
- `ALTER TABLE … ADD CONSTRAINT … PRIMARY KEY / UNIQUE / EXCLUDE`, or `ADD COLUMN` with an inline `PRIMARY KEY` / `UNIQUE` constraint, on such a table, unless the constraint adopts an existing index with `USING INDEX`.

typedpg cannot see how many rows a table has. It treats every table created by an earlier migration file as one that may hold rows in production, and every table created earlier in the same file as empty: a table that did not exist when the file started cannot have rows the migration's lock would block.

## Examples

### Bad

```sql
-- migrations/0001_init.sql
CREATE TABLE users (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    email text NOT NULL,
    name text NOT NULL
);

-- migrations/0002_index_email.sql
CREATE INDEX users_email_idx ON users (email);
ALTER TABLE users ADD CONSTRAINT users_name_key UNIQUE (name);
```

```text
warning[create_index_blocking]: CREATE INDEX without CONCURRENTLY blocks writes on public.users for the whole build
   ╭─[migrations/0002_index_email.sql:1:1]
 1 │ CREATE INDEX users_email_idx ON users (email);
   · ─────────────────────────────────────────────
   ╰────
  help: use CREATE INDEX CONCURRENTLY, in a migration whose first line is -- no-transaction

warning[create_index_blocking]: adding the constraint builds its index under an ACCESS EXCLUSIVE lock, blocking reads and writes on public.users for the whole build
   ╭─[migrations/0002_index_email.sql:2:1]
 2 │ ALTER TABLE users ADD CONSTRAINT users_name_key UNIQUE (name);
   · ─────────────────────────────────────────────────────────────
   ╰────
  help: CREATE UNIQUE INDEX CONCURRENTLY first (in a migration whose first line is -- no-transaction), then ADD CONSTRAINT … UNIQUE / PRIMARY KEY USING INDEX, which only takes the lock briefly
```

The first statement also gets a [`missing_lock_timeout`](missing_lock_timeout.md) finding (not shown).

### Good

Build the index concurrently, in its own migration that opts out of the transaction:

```sql
-- migrations/0002_index_title.sql
-- no-transaction
CREATE INDEX CONCURRENTLY posts_title_idx ON posts (title);
```

For a constraint, build its unique index concurrently first, then attach it. `ADD CONSTRAINT … USING INDEX` still takes `ACCESS EXCLUSIVE`, but only for a catalog update:

```sql
-- migrations/0003_unique_name_index.sql
-- no-transaction
CREATE UNIQUE INDEX CONCURRENTLY users_name_key ON users (name);

-- migrations/0004_unique_name.sql
SET LOCAL lock_timeout = '5s';
ALTER TABLE users ADD CONSTRAINT users_name_key UNIQUE USING INDEX users_name_key;
```

Not reported: an index on a table created in the same file, which has no rows yet.

```sql
-- migrations/0005_tags.sql
CREATE TABLE tags (id bigint PRIMARY KEY, label text NOT NULL);
CREATE INDEX tags_label_idx ON tags (label);
```

If a concurrent build fails (a deadlock, or a duplicate key for a unique index), PostgreSQL leaves an `INVALID` index behind; drop it with `DROP INDEX CONCURRENTLY` and run the build again.

## When it stays silent

- The table was created in the same migration file.
- The file is unchanged from `--previous-deployment <ref>` or matches `lint.ignore`: its statement rules are skipped, because a migration already applied in production cannot be changed.
- The file belongs to `extra_migrations`: it builds the schema but is never linted.
- `CREATE INDEX` on a materialized view is not reported by this rule (it only looks at tables).

## Configuration

Set the level, or turn the rule off:

```toml
# Cargo.toml (cargo typedpg lint)
[package.metadata.typedpg.lint]
create_index_blocking = "error"   # "error", "warning" (default), "info" or "off"
```

```json
// typedpg.config.json (npx typedpg lint)
{ "lint": { "createIndexBlocking": "error" } }
```

To accept one statement, put an allow comment before it (or inside it); it covers that statement only:

```sql
-- typedpg-lint: allow(create_index_blocking)
CREATE INDEX users_email_idx ON users (email);
```

`fail_on` / `failOn` decides which severity fails the run, and `ignore` skips the statement rules of matching migration files. See [Configuring rules](README.md#configuring-rules).

## Related rules

- [`missing_lock_timeout`](missing_lock_timeout.md): the same statement waiting on a lock with no timeout.
- [`drop_index_blocking`](drop_index_blocking.md): the `DROP INDEX` counterpart.
- [`blocking_rebuild`](blocking_rebuild.md): `REINDEX` without `CONCURRENTLY`.
- [`fk_without_index`](fk_without_index.md): the index you are probably adding.
