# `prefer_text`

A `varchar(n)` or `char(n)` column, where `text` (with a `CHECK` if the limit matters) is simpler and no slower.

| | |
|---|---|
| Default level | off (reports at info when enabled with a level) |
| Applies to | the final schema |
| Fix | no |

## Why

In PostgreSQL, `text`, `varchar` and `varchar(n)` share the same storage and the same performance; the length limit is only a check on input. That check is a poor one:

- Raising the limit is easy (`varchar(100)` → `varchar(200)` doesn't rewrite), but lowering it rewrites the table, and the limit lives in the column's type, so every view and function that depends on the column gets in the way of changing it.
- The number is usually arbitrary (`varchar(255)` is a MySQL habit) and the error when it is exceeded comes from the database rather than your validation.

`char(n)` is worse: it pads values with spaces to `n` characters, and the padding is ignored in some comparisons and kept in others, which surprises everyone.

A `text` column with `CHECK (char_length(col) <= n)` keeps a limit that can be changed by swapping the constraint (`NOT VALID` then `VALIDATE`, without a blocking scan).

Off by default: a length limit in the type is a legitimate choice.

## What it reports

Every column of an ordinary or partitioned table in a user schema of the final schema whose type is `varchar(n)` or `char(n)` — including a bare `char`, which is `char(1)` — directly, through a domain, or as an array element type; the length is read through domains and arrays. An unbounded `varchar` is fine and not reported. Partitions and inheritance children are skipped.

For an array column the message shows the array type (`is varchar(20)[]`) and the help suggests `text[]` alone: `char_length` takes one string, so there is no `CHECK` on the column as simple to offer (a domain over `text` with the check, as the element type, keeps a limit).

### Placement and `--previous-deployment`

A schema rule: it reads the columns after the last migration and reports on the last statement that gave the column its type (`CREATE TABLE`, `ADD COLUMN`, `ALTER COLUMN … TYPE`). When that statement is in a migration file unchanged from `--previous-deployment <ref>` (or matching `lint.ignore`), the finding stays there with the help line ending in `, in a new migration`. Columns from `extra_migrations` are not reported.

## Examples

### Bad

With the rule enabled (`prefer_text = "info"`):

```sql
-- migrations/0001_init.sql
CREATE TABLE users (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    email varchar(255) NOT NULL,
    country char(2) NOT NULL,
    tags varchar(20)[]
);
```

```text
info[prefer_text]: public.users.email is varchar(255)
   ╭─[migrations/0001_init.sql:1:1]
 1 │ CREATE TABLE users (
   · ────────────────────
 2 │     id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
   · ───────────────────────────────────────────────────────
 3 │     email varchar(255) NOT NULL,
   · ────────────────────────────────
   · …
   ╰────
  help: use text, with a CHECK (char_length(email) <= 255) if the limit matters

info[prefer_text]: public.users.country is char(2): char pads with spaces
   ╭─[migrations/0001_init.sql:1:1]
 1 │ CREATE TABLE users (
   · ────────────────────
 2 │     id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
   · ───────────────────────────────────────────────────────
 3 │     email varchar(255) NOT NULL,
   · ────────────────────────────────
   · …
   ╰────
  help: use text, with a CHECK (char_length(country) <= 2) if the limit matters

info[prefer_text]: public.users.tags is varchar(20)[]
   ╭─[migrations/0001_init.sql:1:1]
 1 │ CREATE TABLE users (
   · ────────────────────
 2 │     id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
   · ───────────────────────────────────────────────────────
 3 │     email varchar(255) NOT NULL,
   · ────────────────────────────────
   · …
   ╰────
  help: use text[]
```

### Good

```sql
CREATE TABLE users (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    email text NOT NULL CHECK (char_length(email) <= 255),
    country text NOT NULL CHECK (char_length(country) = 2),
    -- An unbounded varchar is text by another name.
    nickname varchar
);
```

## When it stays silent

- The rule is off (the default).
- The column is `text`, unbounded `varchar`, or not a character type.
- The column is in a partition, an inheritance child or a table of a `pg_*` schema or `information_schema`, or comes from `extra_migrations`.
- An allow comment on the statement that gave the column its type (it covers every column of that statement).

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
prefer_text = "info"   # "error", "warning", "info" or "off" (default)
```

```json
// typedpg.config.json
{ "lint": { "preferText": "info" } }
```

For one statement:

```sql
-- typedpg-lint: allow(prefer_text)
CREATE TABLE currencies (code char(3) PRIMARY KEY);
```

See [Configuring rules](README.md#configuring-rules) for `fail_on` / `failOn`, `ignore` and `--previous-deployment`.

## Related rules

- [`fk_type_mismatch`](fk_type_mismatch.md): `text` referencing `varchar` is a type mismatch, so change both sides.
- [`prefer_timestamptz`](prefer_timestamptz.md), [`prefer_identity`](prefer_identity.md), [`prefer_bigint_pk`](prefer_bigint_pk.md), [`prefer_jsonb`](prefer_jsonb.md)
