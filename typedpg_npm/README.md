# typedpg for TypeScript

Typed PostgreSQL queries for TypeScript, checked against your migrations —
no database needed at build time. Write plain SQL; `typedpg gen` analyzes
every query with the same engine as [typedpg for Rust](https://github.com/cubos/typedpg)
(PostgreSQL 18's own parser, an in-memory catalog built from your
migrations, nullability inference through joins, aggregates and
conditions) and generates the module that types and runs them.

```ts
import { sql } from "./db";

const user = await sql("SELECT id, name, email FROM users WHERE id = $id").fetchOne(pool, { id: 42 });
//    ^? { id: number; name: string; email: string | null }
```

A query with an error is a TypeScript error at the query, with PostgreSQL's
message:

```
error TS2345: Argument of type '"SELECT nmae FROM users"' is not assignable to
parameter of type '"typedpg: column \"nmae\" does not exist"'.
```

## Install

```sh
npm install @cubos/typedpg
npm install pg            # or: npm install postgres
```

The `typedpg` command is a native binary, installed for your platform as an
optional dependency (`@cubos/typedpg-cli-linux-x64-gnu`, `-darwin-arm64`, …).
Node.js 22.12 or later.

## Configure

`typedpg.config.json`, next to your `package.json`:

```json
{
  "include": ["src"],
  "out": "src/db.ts",
  "migrations": "migrations"
}
```

| Key | |
|-----|---|
| `include` | Directories (or files) whose `.ts`/`.tsx`/`.mts`/`.cts` sources are scanned. Default `["."]`; `node_modules` and hidden directories are skipped. |
| `out` | The generated module. `.ts`/`.mts`/`.cts` generates TypeScript; `.js`/`.mjs` an ES module and its `.d.ts`; `.cjs` a CommonJS module and its `.d.cts`. |
| `migrations` | The migrations directory (default `migrations`), or `{ "dir", "table", "lockId", "useTransaction", "failOnDrift", "embed" }` — the runner's settings, as in Rust. `"embed": true` adds the migrations to the generated module. |
| `extraMigrations` | More directories the schema is built from but the runner doesn't apply (tables another project owns). |
| `types` | PG type → TypeScript type, as `"./src/types#Prefs"` (relative to the config) or `"package#Type"`. A JSONB domain, an enum, a composite or any other type; an unqualified name is in `public`, a built-in type is `pg_catalog.<name>`. See [Mapping types](#mapping-types). |
| `int8` | `"bigint"` (default, exact), `"string"` (exact, as `pg` returns it) or `"number"` (an error beyond 2^53 instead of a rounded value). |
| `runtime` | The module the generated code imports the runtime from (default `@cubos/typedpg`). |
| `databases` | Several databases: `{ "main": { "out": …, "migrations": … }, "warehouse": { … } }`, each with the keys above. A query belongs to the database whose module its `sql` comes from. |

Unknown keys are errors.

## Generate

```sh
npx typedpg gen            # write the generated modules
npx typedpg gen --watch    # and keep them up to date
npx typedpg check          # in CI: fail if a module is stale or a query has an error
```

`gen` prints each error at its place in your sources, the line and column of
the offending token inside the SQL:

```
src/users.ts:12:25: error: column "nmae" does not exist
  ╭────
1 │ SELECT id, nmae FROM users
  ·            ──┬─
  ·              ╰─ column does not exist
  ╰────
  help: Perhaps you meant to reference the column "users.name".
```

`--watch` rescans only the files that change and analyzes only new queries;
a migration rebuilds that database's schema; a module is rewritten only
when its content changes. Commit the generated module (it is deterministic,
sorted by query) or generate it in your build — `check` keeps either honest.

## Queries

`sql` takes a string literal (or a template literal without `${}`) — that
is what gets analyzed. Parameters are named, `$name`, and passed as an
object:

```ts
import { sql } from "./db";

await sql("SELECT * FROM users").fetchAll(pool);                        // Row[]
await sql("SELECT * FROM users WHERE id = $id").fetchOne(pool, { id }); // Row, or NoRowsError / TooManyRowsError
await sql("SELECT * FROM users WHERE id = $id").fetchOptional(pool, { id }); // Row | null
await sql("SELECT count(*) AS n FROM users").fetchValue(pool);          // bigint — single-column queries
await sql("SELECT email FROM users WHERE id = $id").fetchValueOptional(pool, { id });
await sql("UPDATE users SET name = $name WHERE id = $id").execute(pool, { name, id }); // rows affected

for await (const user of sql("SELECT * FROM users").fetchStream(pool, {}, { batchSize: 500 })) {
  // rows as the server sends them, through a cursor
}
```

The methods mirror the Rust crate's (`fetch_all`, `fetch_one`, …). The SQL
can be anything PostgreSQL accepts: CTEs, window functions, `LATERAL`,
`RETURNING`, `ON CONFLICT`, `DISTINCT ON`…

- **Nullability** is inferred: a `NOT NULL` column is `T`, a nullable one
  `T | null`, and joins, `COALESCE`, `CASE`, aggregates and `WHERE x IS NOT
  NULL` are followed. Override it with an alias, `AS "title!"` (not null) /
  `AS "title?"` (nullable), or on a parameter, `$name?` / `$name!`.
- **Bulk insert**: `INSERT INTO users (name, email) VALUES $..rows { name, email }`,
  with `rows` an array of `{ name, email }`. With no row the query still
  runs, as SQL would: an empty spread next to other rows is left out, and a
  `VALUES` of nothing but empty spreads is written as a `SELECT` of no row
  (PostgreSQL has no `VALUES` without one).
- **Lists**: `WHERE id IN $..ids`, with `ids` an array, is `id IN ($1, $2, …)`.
  An empty list is `(SELECT NULL::<type> WHERE false)` — PostgreSQL has no
  syntax for one — and the query runs: `IN` it is false, `NOT IN` it true.
  `id = ANY($ids)` is the same filter with one array parameter. The drivers
  don't reuse prepared statements (node-postgres sends unnamed ones, and
  postgres.js's `unsafe()` doesn't prepare), so PostgreSQL plans each query
  with its values and the two plan alike; an `IN` list differs only under a
  generic plan — a `Driver` of your own reusing a prepared statement —
  where PostgreSQL prunes partitions only for it and estimates its rows
  from its length instead of assuming 10 elements.
- **Large spreads**: above 1000 items, a list is bound as one array —
  `(id = ANY($1::type[]))` — and a `VALUES` of one rows spread as `SELECT *
  FROM unnest($1::type1[], …)`, a parameter per field: faster, and past
  PostgreSQL's 65535-parameter limit. Only where the analyzer proves it the
  same query (same columns, types and nullability); otherwise, as written.
- **Reusing a query's types**: `Row<typeof q>`, `Params<typeof q>`,
  `CopyRow<typeof c>`.

### COPY

```ts
import { copyIn } from "./db";

const copied = await copyIn("users (name, email)").execute(pool, rows);
```

Streams the rows through `COPY ... FROM STDIN`: an array, any iterable, or
an async iterable, encoded one at a time. A failing row (or source) aborts
the COPY and no row is kept. On node-postgres it needs `pg-copy-streams`.

## Executors and drivers

Pass what your application already has:

| Executor | Notes |
|----------|-------|
| node-postgres `Pool`, `Client`, `PoolClient` | `fetchStream` needs `pg-cursor`, `copyIn` `pg-copy-streams` (optional peer dependencies). A pool takes a client for a stream or a COPY. |
| postgres.js `sql`, a transaction's `sql`, a reserved connection | |
| a `Driver` of your own | Implement `typedpgQuery(text, params)` (and the optional methods for streams, COPY and migrations). |

Transactions are the driver's: run queries on the `PoolClient` inside your
`BEGIN`/`COMMIT`, or on postgres.js's `sql.begin(async (tx) => …)`.

Every value is read in PostgreSQL's text format with the driver's own type
parsing off, and decoded by typedpg: the types are what the generated module
says, whatever `pg.types.setTypeParser` your application set up.

## Types

| PostgreSQL | TypeScript |
|------------|------------|
| `bool` | `boolean` |
| `int2`, `int4`, `float4`, `float8`, `oid` | `number` |
| `int8` | `bigint` (see `int8`); parameters also take `number` and `string` |
| `numeric` | `string` (exact); parameters also take `number` |
| `text`, `varchar`, `char`, `uuid`, `date`, `time`, `timestamp`, `timetz`, `inet`, `money`, … | `string` |
| `timestamptz` | `Date`; parameters also take `string` |
| `interval` | `Interval` (`{ months, days, microseconds: bigint }`, as PostgreSQL stores it); parameters also take `string` |
| `json`, `jsonb` | `JsonValue`; parameters take `unknown` (stringified) |
| `bytea` | `Uint8Array` |
| enum | the union of its labels: `"draft" \| "published"` |
| domain | its base type, or the type `types` maps it to |
| composite, `ROW(...)` | an object with a property per field, each with its nullability |
| array | `T[]`, or `(T \| null)[]` unless the elements are known not null |
| range, multirange | `Range<T>`, `Range<T>[]` (`range(1, 10)`, `emptyRange` build them) |
| `vector`, `halfvec` (pgvector) | `number[]` |
| `hstore` | `Record<string, string \| null>` |
| anything else | `string`, its text form |

A column the query proves holds one of a few values reads as their literal
union, for text, `varchar`, the integer types, `boolean` and enums: a `CASE`
of literals, a column whose table or domain says `CHECK (kind IN ('a', 'b'))`,
an enum or text column narrowed by the `WHERE` (`mood <> 'neutral'`), a
constant:

```ts
const kinds = sql(`
  SELECT CASE WHEN visits > 10 THEN 'regular' ELSE 'new' END AS kind, mood
  FROM users WHERE mood <> 'neutral'
`);
// row: { kind: "new" | "regular"; mood: "happy" | "sad" }
```

Parameters still take any value of their type, and a type `types` maps
keeps its mapping.

Composite values can be parameters (from their text form); an anonymous
record can't, as PostgreSQL has no input for it. In a `json`/`jsonb`
parameter, `null` is SQL NULL, not the JSON `null`.

### Mapping types

A type `types` maps a PG type to changes what the generated module says,
not how values are read: they are decoded as the table above says. So the
type must fit that — a narrower one is fine (a branded `string`, a string
`enum` for an enum, an interface for a composite, any type for JSON), a
different one isn't: mapping a `halfvec` to `string` is a TypeScript error
in the generated module, as its values are `number[]`.

```
src/db.ts(9,16): error TS2344: Type 'string' does not satisfy the constraint 'readonly number[]'.
```

In a `.d.ts` (a JavaScript `out`), `skipLibCheck` skips that check.

## Migrations

The runner is the Rust crate's — the same tracking table, advisory lock,
per-migration transaction (`-- no-transaction` on a migration's first line
runs its statements one at a time, for `CREATE INDEX CONCURRENTLY`) and
drift check — so `typedpg migrate`, `cargo typedpg migrate` and the
functions below manage the same database.

```sh
npx typedpg migrate create add_posts   # migrations/<timestamp>_add_posts.sql + .down.sql
npx typedpg migrate up                 # DATABASE_URL (a .env is read), or --url
npx typedpg migrate status
npx typedpg migrate down [name] [--force]
npx typedpg migrate up --db warehouse  # with several databases
```

At startup, from the generated module (`"migrations": { "embed": true }`)
or from a directory:

```ts
import { migrate, migrationStatus, revertMigration, loadMigrations } from "@cubos/typedpg";
import { migrations } from "./db"; // or: loadMigrations("./migrations")

const applied = await migrate(pool, migrations);
```

Pass a pool or a client, not a transaction: each migration runs in its own.

## Limits

- Dynamic SQL — building a query out of pieces at runtime — is out of
  scope: a query must be a literal for typedpg to analyze it. Use
  `$x IS NULL OR …` conditions, or several queries.
- Values are read in PostgreSQL's default `DateStyle` (ISO) and
  `IntervalStyle` (`postgres` or `iso_8601`); another one is a decode error
  that says so. Multidimensional arrays are an error, as in Rust.
- Node.js 24 and 26 have a `JSON.parse` bug (V8): after any `JSON.parse` in
  the process has read the key `"\\"`, keys of one escaped character
  (`"\t"`, `"\n"`, …) come back as `"\\"`. typedpg's JSON decoding works
  around it; your own `JSON.parse` calls are affected.

## License

MIT OR Apache-2.0.
