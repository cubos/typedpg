# `prefer_jsonb`

A `json` column instead of `jsonb`.

| Level | Applies to | Fix |
|---|---|---|
| off | the final schema | — |

## Example

```sql
-- migrations/002_webhooks.sql
CREATE TABLE webhooks (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    payload json NOT NULL
);
```

```
info[prefer_jsonb]: public.webhooks.payload is json: it has no equality operator, so DISTINCT, GROUP BY, UNION and = fail on it, and every read parses the text again
   ╭─[migrations/002_webhooks.sql:1:1]
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

When the migration is unchanged from `--previous-deployment`, the help reads
`use jsonb in a new migration`.

## Why

`json` stores the input text and parses it on every access. It has no
equality operator, so `DISTINCT`, `GROUP BY`, `UNION` and `=` fail on it,
and no GIN operator class, so `@>` and `?` can't use an index. `jsonb` is
parsed once, comparable and indexable.

Off by default: `json` is right when the exact text matters, e.g. a webhook
body whose signature you verify.

## Reported when

A table column's type is `json`, directly, through a domain, or as an array
element (`json[]`). The finding is placed on the last statement that gave
the column its type (`CREATE TABLE`, `ADD COLUMN`, `ALTER COLUMN … TYPE`).

## Not reported

- Partitions and inheritance children (the parent is reported), tables in
  `pg_*` schemas or `information_schema`, columns from `extra_migrations`.
- A column created by a statement with an allow comment. The comment covers
  the whole statement, so give a deliberate `json` column its own
  `ALTER TABLE … ADD COLUMN`:

  ```sql
  -- typedpg-lint: allow(prefer_jsonb)
  ALTER TABLE webhooks ADD COLUMN raw_body json;
  ```

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
prefer_jsonb = "info"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "preferJsonb": "info" } }
```

Allow one statement by putting `-- typedpg-lint: allow(prefer_jsonb)` before
it in the migration file. See [Configuring rules](README.md#configuring-rules).

## Related

- [`prefer_timestamptz`](prefer_timestamptz.md),
  [`prefer_identity`](prefer_identity.md), [`prefer_text`](prefer_text.md),
  [`prefer_bigint_pk`](prefer_bigint_pk.md): the other opt-in schema
  conventions.
