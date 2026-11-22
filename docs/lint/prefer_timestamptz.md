# `prefer_timestamptz`

A `timestamp` (without time zone) column instead of `timestamptz`.

| Level | Applies to | Fix |
|---|---|---|
| off | the final schema | — |

## Example

```sql
-- migrations/004_logs.sql
CREATE TABLE logs (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    happened_at timestamp NOT NULL,
    reminders timestamp[]
);
```

```
info[prefer_timestamptz]: public.logs.happened_at is timestamp without time zone: it stores no offset, so the same instant reads differently per session TimeZone
   ╭─[migrations/004_logs.sql:1:1]
 1 │ CREATE TABLE logs (
   · ───────────────────
 2 │     id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
   · ───────────────────────────────────────────────────────
 3 │     happened_at timestamp NOT NULL,
   · ───────────────────────────────────
   · …
   ╰────
  help: use timestamptz
```

`reminders` gets the same finding. When the migration is unchanged from
`--previous-deployment`, the help reads
`use timestamptz in a new migration, rewriting the values as it goes`.

## Why

`timestamptz` stores an instant; `timestamp` stores clock digits with no
offset. Comparing a `timestamp` with `now()`, or assigning between the two,
converts through the session's `TimeZone`, so the same query returns
different rows on a server, a laptop and a CI runner. `timestamp` fits
deliberately zone-less values, such as a store's opening time.

Off by default: many schemas store UTC in `timestamp` by convention.

## Reported when

A table column's type is `timestamp`, directly, through a domain, or as an
array element (`timestamp[]`). The finding is placed on the last statement
that gave the column its type (`CREATE TABLE`, `ADD COLUMN`,
`ALTER COLUMN … TYPE`); a later `ALTER COLUMN … TYPE timestamptz` resolves
it.

## Not reported

- `timestamptz`, `date`, `time`.
- Partitions and inheritance children (the parent is reported), tables in
  `pg_*` schemas or `information_schema`, columns from `extra_migrations`.

Converting a column with `ALTER COLUMN … TYPE timestamptz` rewrites the
table ([`table_rewrite`](table_rewrite.md)) and reads the old values in the
migration session's `TimeZone`, or in the one
`USING happened_at AT TIME ZONE 'UTC'` names.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
prefer_timestamptz = "info"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "preferTimestamptz": "info" } }
```

Allow one statement by putting `-- typedpg-lint: allow(prefer_timestamptz)`
before it in the migration file; it covers every column of that statement.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`timestamp_timezone_mix`](timestamp_timezone_mix.md): queries where a
  `timestamp` meets a `timestamptz`.
- [`prefer_identity`](prefer_identity.md), [`prefer_text`](prefer_text.md),
  [`prefer_bigint_pk`](prefer_bigint_pk.md), [`prefer_jsonb`](prefer_jsonb.md):
  the other opt-in schema conventions.
