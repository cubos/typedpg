# `prefer_bigint_pk`

A primary key column of type `smallint` (`int2`) or `integer` (`int4`), which runs out at 32767 or 2147483647.

| | |
|---|---|
| Default level | off (reports at info when enabled with a level) |
| Applies to | the final schema |
| Fix | no |

## Why

Two billion sounds like a lot until a table that gets rows from events, logs or retries hits it. Sequences also skip values (rolled-back inserts, cached sequence values, `ON CONFLICT` upserts consume ids even when they don't insert), so ids run out faster than row counts suggest. When the key is exhausted, every insert fails, and the fix — `ALTER COLUMN id TYPE bigint` on the table and on every column referencing it — rewrites all of those tables under an `ACCESS EXCLUSIVE` lock, at the moment the table is at its largest.

`bigint` costs four more bytes per row and per index entry; for most tables that is cheaper than the migration.

Off by default: a small lookup table with a `smallint` key is perfectly reasonable.

## What it reports

Every column of a `PRIMARY KEY` constraint (each column of a composite key) on an ordinary or partitioned table in a user schema of the final schema whose type is `int2` or `int4`, directly or through a domain. Partitions and inheritance children are skipped; a `serial` key is an `int4` key and is reported.

### Placement and `--previous-deployment`

A schema rule: it reads the keys after the last migration and reports on the last statement that gave the column its type (`CREATE TABLE`, `ADD COLUMN`, `ALTER COLUMN … TYPE`). When that statement is in a migration file unchanged from `--previous-deployment <ref>` (or matching `lint.ignore`), the finding stays there with the help line `use bigint in a new migration`. Columns from `extra_migrations` are not reported.

## Examples

### Bad

With the rule enabled (`prefer_bigint_pk = "info"`):

```sql
-- migrations/0001_init.sql
CREATE TABLE users (
    id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    email text NOT NULL
);
CREATE TABLE countries (code smallint PRIMARY KEY);
```

```text
info[prefer_bigint_pk]: public.users.id is an int4 primary key: it runs out at 2147483647
   ╭─[migrations/0001_init.sql:1:1]
 1 │ CREATE TABLE users (
   · ────────────────────
 2 │     id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
   · ────────────────────────────────────────────────────────
 3 │     email text NOT NULL
   · ───────────────────────
 4 │ );
   · ─
   ╰────
  help: use bigint

info[prefer_bigint_pk]: public.countries.code is an int2 primary key: it runs out at 32767
   ╭─[migrations/0001_init.sql:5:1]
 5 │ CREATE TABLE countries (code smallint PRIMARY KEY);
   · ──────────────────────────────────────────────────
   ╰────
  help: use bigint
```

### Good

```sql
CREATE TABLE users (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    email text NOT NULL
);
-- Not an integer key.
CREATE TABLE countries (code text PRIMARY KEY);
```

Only primary keys are checked: an `integer` column with a `UNIQUE` constraint is not reported (though the table may then get [`missing_primary_key`](missing_primary_key.md)).

## When it stays silent

- The rule is off (the default).
- The key is `bigint` or not an integer type, or the column is not part of a primary key.
- The table is a partition, an inheritance child or in a `pg_*` schema or `information_schema`, or comes from `extra_migrations`.
- An allow comment on the statement that gave the column its type (it covers every column of that statement).

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
prefer_bigint_pk = "info"   # "error", "warning", "info" or "off" (default)
```

```json
// typedpg.config.json
{ "lint": { "preferBigintPk": "info" } }
```

For one statement:

```sql
-- typedpg-lint: allow(prefer_bigint_pk)
CREATE TABLE countries (code smallint PRIMARY KEY);
```

See [Configuring rules](README.md#configuring-rules) for `fail_on` / `failOn`, `ignore` and `--previous-deployment`.

## Related rules

- [`fk_type_mismatch`](fk_type_mismatch.md): changing the key's type means changing every referencing column too.
- [`prefer_identity`](prefer_identity.md), [`missing_primary_key`](missing_primary_key.md)
- [`prefer_timestamptz`](prefer_timestamptz.md), [`prefer_text`](prefer_text.md), [`prefer_jsonb`](prefer_jsonb.md)
