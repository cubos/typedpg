# `json_stringify_param`

`JSON.stringify(…)` passed as the value of a `json` / `jsonb` parameter.
TypeScript only.

| Level | Applies to | Fix |
|---|---|---|
| error | call sites | — |

## Example

```sql
CREATE TABLE documents (id integer PRIMARY KEY, body jsonb NOT NULL);
```

```ts
// src/documents.ts
import { sql } from "./db";

await sql("INSERT INTO documents (id, body) VALUES ($id, $body)")
  .execute(pool, { id: 1, body: JSON.stringify(doc) });
```

```
error[json_stringify_param]: parameter $body is jsonb, and `JSON.stringify(…)` stores a JSON string, not the value
   ╭─[src/documents.ts:5:33]
 5 │   .execute(pool, { id: 1, body: JSON.stringify(doc) });
   ·                                 ───────────────────
   ╰────
  help: pass the value itself: typedpg encodes it as JSON (`jsonNull` for a JSON null)
```

Pass `{ id: 1, body: doc }`.

## Why

typedpg encodes a `json` / `jsonb` value itself, and a string is a valid
JSON value, so the call type-checks. The column then holds the JSON string
`"{\"title\":\"hello\"}"`, not the object: `body ->> 'title'` is NULL, and
reading the row gives back a string. With node-postgres (and pgtyped over
it) a string was the parameter's text, which PostgreSQL parsed as JSON, so
code moved from it often still has the call.

## Reported when

The value is a `JSON.stringify(…)` call, written directly (also inside
parentheses, `as`, `satisfies`, `!` or `<T>`) at the call:

- a property of the parameters object of `fetchOne`, `fetchOptional`,
  `fetchValue`, `fetchValueOptional`, `fetchAll`, `fetchStream` or
  `execute`, whose parameter is `json`, `jsonb` or a domain over one;
- an element of an array literal passed to a `json[]` / `jsonb[]` parameter
  or an `IN $..list` of `jsonb`;
- a field of an object in an array literal passed to a rows spread
  (`$..rows { id, body }`);
- a field of an object in the array literal of rows passed to a
  `copyIn(…).execute`, for a `json` / `jsonb` column.

The calls seen are those on `sql(…)` or `copyIn(…)`, on a local binding of
it, and on an exported query (from a `.sql` file or a `sql(…)` binding)
imported by name, alias or namespace. Each finding is placed at the
`JSON.stringify(…)` call.

## Not reported

The rule is syntactic: a JSON string reaching the parameter any other way
is not seen.

- A variable holding the string (`const text = JSON.stringify(doc)`), a
  helper that returns one, a parameters object built elsewhere.
- A `text` parameter: storing JSON text there is what the code asks for,
  and so is `$body::text::jsonb`, which parses it.
- A shadowed `JSON` (a local binding of that name).

## Configuration

```json
// typedpg.config.json
{ "lint": { "jsonStringifyParam": "warning" } }
```

Allow one query with `-- typedpg-lint: allow(json_stringify_param)` in its
SQL; it covers every call of that query. A `copyIn` target has no SQL
comments: set the rule's level instead.
See [Configuring rules](README.md#configuring-rules).

## Related

- [`prefer_jsonb`](prefer_jsonb.md): a `json` column instead of `jsonb`.
