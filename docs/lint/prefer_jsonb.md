# `prefer_jsonb`

A `json` column, which stores JSON as text, where `jsonb` stores it parsed.

| | |
|---|---|
| Default level | off (reports at info when enabled with a level) |
| Applies to | the final schema |
| Fix | no |

## Why

`json` keeps the input text exactly as written — whitespace, key order, duplicate keys — and parses it again on every access. It has no equality operator, so `SELECT DISTINCT`, `GROUP BY`, `UNION` (without `ALL`) and `=` fail on it with "could not identify an equality operator for type json", and it has no GIN operator class, so containment queries (`@>`, `?`) can't be indexed. `jsonb` stores a parsed binary form: faster to process, comparable, indexable.

Keep `json` when the exact text matters — a webhook payload whose signature you verify, for example.

Off by default: `json` is a legitimate choice for that case.

## What it reports

Every column of an ordinary or partitioned table in a user schema of the final schema whose type is `pg_catalog.json` — directly, through a domain, or as an array element type (`json[]`). Partitions and inheritance children are skipped.

### Placement and `--previous-deployment`

A schema rule: it reads the columns after the last migration and reports on the last statement that gave the column its type (`CREATE TABLE`, `ADD COLUMN`, `ALTER COLUMN … TYPE`). When that statement is in a migration file unchanged from `--previous-deployment <ref>` (or matching `lint.ignore`), the finding stays there with the help line `use jsonb in a new migration`. Columns from `extra_migrations` are not reported.

## Examples

### Bad

With the rule enabled (`prefer_jsonb = "info"`):

```sql
-- migrations/0001_init.sql
CREATE TABLE webhooks (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    payload json NOT NULL
);
```

```text
info[prefer_jsonb]: public.webhooks.payload is json: it has no equality operator, so DISTINCT, GROUP BY, UNION and = fail on it, and every read parses the text again
   ╭─[migrations/0001_init.sql:1:1]
 1 │ CREATE TABLE webhooks (
   · ───────────────────────
 2 │     id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
   · ───────────────────────────────────────────────────────
 3 │     payload json NOT NULL
   · ─────────────────────────
 4 │ );
   · ─
   ╰────
  help: use jsonb, unless the exact text (key order, duplicate keys, whitespace) must be kept
```

### Good

```sql
CREATE TABLE webhooks (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    payload jsonb NOT NULL
);
```

When the raw text must be kept, allow it on the statement that creates the column. The comment covers the whole statement, so put such a column in its own `ALTER TABLE … ADD COLUMN` if the same `CREATE TABLE` has other `json` columns that should still be reported:

```sql
-- typedpg-lint: allow(prefer_jsonb)
ALTER TABLE webhooks ADD COLUMN raw_body json;
```

## When it stays silent

- The rule is off (the default).
- The column is in a partition, an inheritance child or a table of a `pg_*` schema or `information_schema`, or comes from `extra_migrations`.
- An allow comment on the statement that gave the column its type.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
prefer_jsonb = "info"   # "error", "warning", "info" or "off" (default)
```

```json
// typedpg.config.json
{ "lint": { "preferJsonb": "info" } }
```

See [Configuring rules](README.md#configuring-rules) for `fail_on` / `failOn`, `ignore` and `--previous-deployment`.

## Related rules

- [`prefer_timestamptz`](prefer_timestamptz.md), [`prefer_identity`](prefer_identity.md), [`prefer_text`](prefer_text.md), [`prefer_bigint_pk`](prefer_bigint_pk.md): the other opt-in schema conventions.
