# `prefer_bigint_pk`

A `smallint` or `integer` primary key column, which runs out at 32767 or
2147483647.

| Level | Applies to | Fix |
|---|---|---|
| off | final schema | — |

## Example

With `prefer_bigint_pk = "info"`:

```sql
-- migrations/0001_init.sql
CREATE TABLE users (id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY, email text NOT NULL);
CREATE TABLE countries (code smallint PRIMARY KEY);
```

```
info[prefer_bigint_pk]: public.users.id is an int4 primary key: it runs out at 2147483647
   ╭─[migrations/0001_init.sql:1:1]
 1 │ CREATE TABLE users (id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY, email text NOT NULL);
   · ─────────────────────────────────────────────────────────────────────────────────────────────
   ╰────
  help: use bigint

info[prefer_bigint_pk]: public.countries.code is an int2 primary key: it runs out at 32767
   ╭─[migrations/0001_init.sql:2:1]
 2 │ CREATE TABLE countries (code smallint PRIMARY KEY);
   · ──────────────────────────────────────────────────
   ╰────
  help: use bigint
```

## Why

Sequences skip values (rollbacks, caching, `ON CONFLICT`), so ids run out
sooner than row counts suggest, and then every insert fails. Changing the
key to `bigint` later rewrites the table and every referencing table under
an `ACCESS EXCLUSIVE` lock, when they are largest. Off by default: a small
lookup table with a `smallint` key is fine.

## Reported when

A column of a `PRIMARY KEY` (each column of a composite one) is `int2` or
`int4` after the last migration, directly or through a domain. A `serial`
key is an `int4` key.

The finding goes on the last statement that gave the column its type
(`CREATE TABLE`, `ADD COLUMN`, `ALTER COLUMN … TYPE`). In a file unchanged
since `--previous-deployment` or matched by `ignore`, the help reads
`use bigint in a new migration`.

## Not reported

- `bigint` or non-integer keys; integer columns outside a primary key, even
  `UNIQUE` ones.
- Partitions and inheritance children; tables in `pg_*` schemas or
  `information_schema`.
- Columns created by `extra_migrations`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
prefer_bigint_pk = "info"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "preferBigintPk": "info" } }
```

Allow one statement's columns with `-- typedpg-lint: allow(prefer_bigint_pk)`
before it in the migration file.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`fk_type_mismatch`](fk_type_mismatch.md): changing the key's type means
  changing every referencing column too.
- [`missing_primary_key`](missing_primary_key.md): a table without a key.
- [`prefer_identity`](prefer_identity.md): an identity column instead of
  `serial`.
