# `prefer_timestamptz`

A column of type `timestamp` (`timestamp without time zone`), which stores a wall-clock reading with no offset, where `timestamptz` stores an instant.

| | |
|---|---|
| Default level | off (reports at info when enabled with a level) |
| Applies to | the final schema |
| Fix | no |

## Why

Despite the names, neither type stores a time zone. `timestamptz` stores an absolute instant (UTC internally) and converts it to and from the session's `TimeZone` setting on input and output. `timestamp` stores the digits it was given and nothing else: `2026-03-08 02:30` could be any of two dozen instants, depending on which zone the writer meant.

The trouble starts when the two meet. Comparing a `timestamp` column with `now()` (a `timestamptz`), or assigning one to the other, converts using the session's `TimeZone` — so the same query returns different rows on a server, a laptop and a CI runner with different settings. Daylight-saving transitions add hours that don't exist and hours that happen twice. For "when did this happen", `timestamptz` is almost always what you want; `timestamp` fits wall-clock values that are deliberately zone-less (a store's opening time, a birthday at midnight local time).

The rule is off by default because it is a style choice, not a bug: plenty of schemas use `timestamp` in UTC by convention.

## What it reports

Every column of an ordinary or partitioned table in a user schema of the final schema whose type is `pg_catalog.timestamp` — directly, through a domain, or as the element type of an array (`timestamp[]`). Partitions and inheritance children are skipped: their parent's column already carries the finding.

### Placement and `--previous-deployment`

A schema rule: it reads the columns after the last migration and reports on the last statement that gave the column its type — the `CREATE TABLE`, `ADD COLUMN` or `ALTER COLUMN … TYPE`. A later `ALTER COLUMN … TYPE timestamptz` resolves it, and a later `ALTER COLUMN … TYPE timestamp` is where it is reported. When that statement is in a migration file unchanged from `--previous-deployment <ref>` (or matching `lint.ignore`), the finding stays there with the help line `use timestamptz in a new migration, rewriting the values as it goes`. Columns created by `extra_migrations` are not reported.

## Examples

### Bad

With the rule enabled (`prefer_timestamptz = "info"`):

```sql
-- migrations/0001_init.sql
CREATE TABLE events (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    happened_at timestamp NOT NULL,
    reminders timestamp[]
);
```

```text
info[prefer_timestamptz]: public.events.happened_at is timestamp without time zone: it stores no offset, so the same instant reads differently per session TimeZone
   ╭─[migrations/0001_init.sql:1:1]
 1 │ CREATE TABLE events (
   · ─────────────────────
 2 │     id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
   · ───────────────────────────────────────────────────────
 3 │     happened_at timestamp NOT NULL,
   · ───────────────────────────────────
   · …
   ╰────
  help: use timestamptz

info[prefer_timestamptz]: public.events.reminders is timestamp without time zone: it stores no offset, so the same instant reads differently per session TimeZone
   ╭─[migrations/0001_init.sql:1:1]
 1 │ CREATE TABLE events (
   · ─────────────────────
 2 │     id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
   · ───────────────────────────────────────────────────────
 3 │     happened_at timestamp NOT NULL,
   · ───────────────────────────────────
   · …
   ╰────
  help: use timestamptz
```

### Good

```sql
CREATE TABLE events (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    happened_at timestamptz NOT NULL,
    reminders timestamptz[],
    -- date and time are not reported.
    day date NOT NULL,
    starts time NOT NULL
);
```

Converting an existing column with `ALTER COLUMN … TYPE timestamptz` rewrites the table ([`table_rewrite`](table_rewrite.md)) and interprets the stored values in the migration session's `TimeZone` (or in the zone a `USING happened_at AT TIME ZONE 'UTC'` names): decide which zone the old values meant.

## When it stays silent

- The rule is off (the default).
- The column is in a partition, an inheritance child or a table of a `pg_*` schema or `information_schema`, or comes from `extra_migrations`.
- An allow comment on the statement that gave the column its type. The comment covers the whole statement, so every column it creates.

## Configuration

Enable it with a level:

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
prefer_timestamptz = "info"   # "error", "warning", "info" or "off" (default)
```

```json
// typedpg.config.json
{ "lint": { "preferTimestamptz": "info" } }
```

To accept the columns of one statement:

```sql
-- typedpg-lint: allow(prefer_timestamptz)
CREATE TABLE store_hours (store_id bigint PRIMARY KEY, opens_at timestamp NOT NULL);
```

See [Configuring rules](README.md#configuring-rules) for `fail_on` / `failOn`, `ignore` and `--previous-deployment`.

## Related rules

- [`timestamp_timezone_mix`](timestamp_timezone_mix.md): the queries where a `timestamp` meets a `timestamptz`.
- [`prefer_identity`](prefer_identity.md), [`prefer_text`](prefer_text.md), [`prefer_bigint_pk`](prefer_bigint_pk.md), [`prefer_jsonb`](prefer_jsonb.md): the other opt-in schema conventions.
