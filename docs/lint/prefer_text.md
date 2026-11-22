# `prefer_text`

A `varchar(n)` or `char(n)` column instead of `text`.

| Level | Applies to | Fix |
|---|---|---|
| off | the final schema | — |

## Example

```sql
-- migrations/003_users.sql
CREATE TABLE users (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    email varchar(255) NOT NULL,
    country char(2) NOT NULL,
    tags varchar(20)[]
);
```

```
info[prefer_text]: public.users.email is varchar(255)
   ╭─[migrations/003_users.sql:1:1]
 1 │ CREATE TABLE users (
   · ────────────────────
 2 │     id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
   · ───────────────────────────────────────────────────────
 3 │     email varchar(255) NOT NULL,
   · ────────────────────────────────
   · …
   ╰────
  help: use text, with a CHECK (char_length(email) <= 255) if the limit matters
```

The other two are reported as `public.users.country is char(2): char pads
with spaces` and `public.users.tags is varchar(20)[]` (help: `use text[]`).
When the migration is unchanged from `--previous-deployment`, the help ends
in `, in a new migration`.

## Why

`text`, `varchar` and `varchar(n)` store and perform the same; the length is
only an input check, and a poor one: lowering it rewrites the table, views
and functions depending on the column get in the way of changing it, and the
number is often arbitrary (`255`). `char(n)` pads with spaces that some
comparisons ignore and others keep. `text` with
`CHECK (char_length(col) <= n)` keeps a limit you can swap without a
rewrite.

Off by default: a length in the type is a legitimate choice.

## Reported when

A table column's type is `varchar(n)` or `char(n)` (a bare `char` is
`char(1)`), directly, through a domain, or as an array element. The finding
is placed on the last statement that gave the column its type
(`CREATE TABLE`, `ADD COLUMN`, `ALTER COLUMN … TYPE`).

## Not reported

- `text` and unbounded `varchar`.
- Partitions and inheritance children (the parent is reported), tables in
  `pg_*` schemas or `information_schema`, columns from `extra_migrations`.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
prefer_text = "info"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "preferText": "info" } }
```

Allow one statement by putting `-- typedpg-lint: allow(prefer_text)` before
it in the migration file; it covers every column of that statement.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`fk_type_mismatch`](fk_type_mismatch.md): `text` referencing `varchar`
  is a mismatch, so change both sides.
- [`prefer_timestamptz`](prefer_timestamptz.md),
  [`prefer_identity`](prefer_identity.md),
  [`prefer_bigint_pk`](prefer_bigint_pk.md), [`prefer_jsonb`](prefer_jsonb.md):
  the other opt-in schema conventions.
