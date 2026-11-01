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
npm install @cubos/typedpg                  # the runtime
npm install pg                              # or: npm install postgres
npm install --save-dev @cubos/typedpg-cli   # the `typedpg` command and the bundler plugins
```

`@cubos/typedpg` is the runtime your application deploys: the query runtime,
codecs, adapters and the migration runner. It holds no native binary. The
`typedpg` command and the bundler plugins live in `@cubos/typedpg-cli`, which
you install as a devDependency (see its
[reference](https://github.com/cubos/typedpg/blob/main/typedpg_npm_cli/README.md)).
The command runs a native binary, installed for your platform as an optional
dependency of the CLI (`@cubos/typedpg-cli-linux-x64-gnu`, `-darwin-arm64`,
…), so `npm install --prod` never downloads it. Node.js 22.12 or later;
the Node loader for `.sql` files needs 22.15 or later.

## Quick start

A migration, `migrations/0001_create_users.sql`:

```sql
CREATE TABLE users (
    id    SERIAL PRIMARY KEY,
    name  TEXT NOT NULL,
    email TEXT UNIQUE,
    age   INT,
    bio   TEXT
);
```

A `typedpg.config.json`, next to your `package.json`:

```json
{
  "include": ["src"],
  "out": "src/db.ts",
  "migrations": "migrations"
}
```

A query, in `src/users.ts`:

```ts
import { Pool } from "pg";
import { sql } from "./db";

const pool = new Pool({ connectionString: process.env.DATABASE_URL });

export function findUser(id: number) {
  return sql("SELECT id, name, email FROM users WHERE id = $id").fetchOne(pool, { id });
}
```

```sh
npx typedpg gen
```

`gen` reads the migrations, analyzes the query and writes `src/db.ts`, whose
`sql` is typed by the query. `npx typedpg check` fails if the module is stale
or a query has an error.

## What typedpg infers

The analyzer behind the Rust crate's `sql!` runs over the schema your
migrations build — the same engine, the same rules. It writes one type per
query into the generated module, and tsc checks every `sql(...)` against it.
Every type below is that module's.

```ts
import { sql } from "./db";
```

### Nullability

A column's own nullability:

```ts
const user = await sql("SELECT id, name, age, bio FROM users WHERE id = $id").fetchOne(pool, { id });
// user: { id: number; name: string; age: number | null; bio: string | null }
```

A `LEFT JOIN` makes its right side nullable; the left side keeps what it had.
A `RIGHT JOIN` is the mirror: `SELECT u.name, p.title FROM posts p RIGHT JOIN
users u ON p.author_id = u.id` gives `{ name: string; title: string | null }`.

```ts
const row = await sql(`
  SELECT u.name, p.title
  FROM users u LEFT JOIN posts p ON p.author_id = u.id
  WHERE u.id = $id
`).fetchOne(pool, { id });
// row: { name: string; title: string | null }
```

`COALESCE` with a non-NULL fallback and a `CASE` covering the NULL are not
nullable, and subqueries, CTEs and `UNION` keep the inference through:

```ts
const row = await sql("SELECT COALESCE(age, 0) AS age FROM users WHERE id = $id").fetchOne(pool, { id });
// row: { age: number }

const cases = await sql("SELECT CASE WHEN age IS NULL THEN 0 ELSE age END AS age FROM users").fetchAll(pool);
// cases: { age: number }[]

const rows = await sql(`
  WITH v AS (SELECT age FROM users WHERE age IS NOT NULL)
  SELECT age FROM v UNION SELECT 21
`).fetchAll(pool);
// rows: { age: number }[]
```

`RETURNING` knows what the statement wrote: an `INSERT` returns the column
defaults, an `UPDATE` its `SET` values and what its `WHERE` proved:

```ts
const row = await sql("INSERT INTO posts (author_id, title) VALUES ($author, $title) RETURNING status").fetchOne(pool, { author, title });
// row: { status: string }   (status is NOT NULL DEFAULT 'draft')

const updated = await sql("UPDATE users SET age = 18 WHERE age IS NULL RETURNING age").fetchOne(pool);
// updated: { age: number }
```

`ON CONFLICT DO UPDATE` returns the inserted or the updated row, and `MERGE`
one row per action it runs, past its `WHEN`. An automatically updatable view
writes its base table's rows, which need not pass its `WHERE`; a BEFORE ROW
trigger or a rule may rewrite the row, so its values are not trusted there
(the constraints are).

The schema's constraints count, taken as declared (`NOT VALID` included;
`NOT ENFORCED` says nothing). A `LEFT JOIN` along a NOT NULL foreign key always
finds its row, and so does a scalar lookup of it:

```ts
const rows = await sql(`
  SELECT p.title, u.name FROM posts p LEFT JOIN users u ON u.id = p.author_id
`).fetchAll(pool);
// rows: { title: string; name: string }[]

const authors = await sql(`
  SELECT p.title, (SELECT u.name FROM users u WHERE u.id = p.author_id) AS author
  FROM posts p
`).fetchAll(pool);
// authors: { title: string; author: string }[]
```

A referenced table under row-level security does not count: its policies can
hide the row. A partition's bound counts too (a range partition key, or a list
one with no NULL, is not nullable), and so does a `MATCH FULL` foreign key:
one of its columns non-NULL makes all of them so.

Some queries always yield a row — an aggregate without `GROUP BY` or
`HAVING`, a query without `FROM`, `VALUES`, such a lookup — so an outer join
`ON true` to one never null-extends it:

```ts
const rows = await sql(`
  SELECT u.name, s.posts FROM users u
  LEFT JOIN LATERAL (SELECT count(*) AS posts FROM posts p WHERE p.author_id = u.id) s ON true
`).fetchAll(pool);
// rows: { name: string; posts: bigint }[]
```

A `FULL JOIN` row has one side or the other, so `coalesce` over a NOT NULL
column of each is NOT NULL:

```ts
const rows = await sql(`
  SELECT coalesce(u.id, p.id) AS id
  FROM users u FULL JOIN posts p ON p.author_id = u.id
`).fetchAll(pool);
// rows: { id: number }[]
```

A data-modifying CTE that inserts one `VALUES` row (without a set-returning
function, `ON CONFLICT DO NOTHING`, a `DO UPDATE ... WHERE` or a BEFORE ROW
trigger, which may skip it) returns exactly one row, so `(SELECT id FROM ins)`
is that row's `id`:

```ts
const row = await sql(`
  WITH ins AS (INSERT INTO posts (author_id, title) VALUES ($author, $title) RETURNING id)
  SELECT (SELECT id FROM ins) AS id
`).fetchOne(pool, { author, title });
// row: { id: number }
```

### Functions and aggregates

What a builtin does with NULL counts, not just whether it is strict:
`format`, `array_remove`, `string_to_array` and `||` on arrays are NULL only
for the arguments that make them so; `jsonb_each` always fills `key` and
`value`; `EXTRACT(epoch FROM interval)` stays non-NULL for an infinite
interval. An aggregate keeping NULL inputs (`array_agg`, `json_agg`,
`JSON_ARRAYAGG`, …) is NULL only over no rows, `rank(…) WITHIN GROUP` and
`regr_count` never are NULL, and an aggregate has rows in a group, in a window
frame holding the current row, over a constant source (`VALUES`,
`generate_series(1, 10)`), and in a group `HAVING` keeps (`HAVING max(x) > 0`;
`HAVING count(x) > 0` also makes `max(x)` non-NULL).

`count` is never NULL, even without `GROUP BY`; `sum`, `avg` and `max` are
NULL when there is no row to read:

```ts
const row = await sql("SELECT count(*) AS rows, sum(age) AS total, max(age) AS oldest FROM users").fetchOne(pool);
// row: { rows: bigint; total: bigint | null; oldest: number | null }
```

`array_agg` keeps its NULL inputs, so its elements are nullable even when the
array is not; `count` over the same `LEFT JOIN` is not:

```ts
const rows = await sql(`
  SELECT u.id, count(p.id) AS posts, array_agg(p.title) AS titles
  FROM users u LEFT JOIN posts p ON p.author_id = u.id
  GROUP BY u.id
`).fetchAll(pool);
// rows: { id: number; posts: bigint; titles: (string | null)[] }[]
```

A `FILTER` reads only the rows it passes, and `HAVING count(age) > 0` proves
the group has a non-NULL age:

```ts
const rows = await sql(`
  SELECT u.id, array_agg(p.title) FILTER (WHERE p.id IS NOT NULL) AS titles
  FROM users u LEFT JOIN posts p ON p.author_id = u.id
  GROUP BY u.id
`).fetchAll(pool);
// rows: { id: number; titles: string[] | null }[]   (no NULL element; NULL with no post)

const sums = await sql("SELECT sum(age) FROM users HAVING count(age) > 0").fetchAll(pool);
// sums: { sum: bigint }[]   (no row, or the sum — never a NULL sum)
```

### Narrowing by conditions

A condition that must hold for a value to be read narrows it, the way
PostgreSQL's planner reasons (`find_nonnullable_vars`, `reduce_outer_joins`):
`WHERE`, `HAVING`, an inner join's `ON`, an aggregate's `FILTER` and a `CASE`
branch's `WHEN`.

```ts
const rows = await sql("SELECT age FROM users WHERE age > $min").fetchAll(pool, { min: 18 });
// rows: { age: number }[]
```

...and it turns a `LEFT JOIN` filtered on its nullable side into an inner one:

```ts
const rows = await sql(`
  SELECT u.name, p.title FROM users u LEFT JOIN posts p ON p.author_id = u.id
  WHERE p.published IS NOT NULL
`).fetchAll(pool);
// rows: { name: string; title: string }[]
```

A condition on an expression narrows that expression where it is read again:

```ts
const rows = await sql("SELECT data ->> 'email' AS email FROM users WHERE data ->> 'email' IS NOT NULL").fetchAll(pool);
// rows: { email: string }[]
```

The schema's CHECK constraints combine with what the query knows, an `iff`
written as equalities, `NOT`, `IS DISTINCT FROM` or `CASE` included, and
refuted by `<>`, `IN` lists, `= ANY (…)` and orderings:

```ts
// CHECK ((kind = 'card' AND card_last4 IS NOT NULL) OR (kind = 'iban' AND iban IS NOT NULL))
const rows = await sql("SELECT kind, card_last4, coalesce(card_last4, iban) AS ref FROM payments WHERE kind = 'card'").fetchAll(pool);
// rows: { kind: "card"; card_last4: string; ref: string }[]

// CHECK ((status = 'done') = (done_at IS NOT NULL))
const tasks = await sql("SELECT done_at FROM tasks WHERE status IN ('done')").fetchAll(pool);
// tasks: { done_at: Date }[]
```

Besides strict conditions, any condition that can't hold with a column NULL
counts: `coalesce(age, 0) > 0`, `age IS NOT DISTINCT FROM 5`, `(tenant_id,
id) = ($1, $2)`, `b IS NULL OR b > 0` (never NULL itself), or an `EXISTS`
whose subquery's `WHERE` is strict in an outer column. Conditions that may
hold for a NULL prove nothing: `coalesce(age, 1) > 0`, `age IS DISTINCT FROM
5`, or an `OR` whose arms test different columns.

```ts
const rows = await sql("SELECT age FROM users WHERE coalesce(age, 0) > 0").fetchAll(pool);
// rows: { age: number }[]
```

Such narrowing follows the expression, not just a column — `HAVING max(x) > 0`
then `max(x)` — unless it runs a volatile function. A generated column (STORED
or VIRTUAL) is its expression over the row, so `GENERATED ALWAYS AS
(coalesce(note, ''))` is never NULL; an automatically updatable view reads its
base table's CHECK constraints.

A strict set-returning function's row in `FROM` (`generate_series(1, t.n)`,
`jsonb_array_elements(t.doc)`) proves its arguments are non-NULL, and a strict
condition on a column a subquery or view passes through narrows the row it
came from.

### Finite dates and times

`to_char` and `EXTRACT` / `date_part` of most fields are NULL for an infinite
date, timestamp or interval. A value is finite when it is the current time, a
literal other than `infinity`, a `timestamp` / `timestamptz` parameter (a
`Date` cannot hold an infinity — unless `types` maps the type), arithmetic,
truncation or a conversion of finite values, the `min` / `max` / `lag` of
them, or a column whose table (or domain) says `CHECK (isfinite(col))`. It
follows the value through subqueries, CTEs, `UNION`, `CASE` and `COALESCE`:

```ts
// events.created_at has CHECK (isfinite(created_at))
const rows = await sql("SELECT to_char(created_at, 'YYYY-MM') AS month FROM events").fetchAll(pool);
// rows: { month: string }[]

const parts = await sql(`
  SELECT to_char($at::timestamptz, 'YYYY-MM') AS month,
         extract(epoch FROM $at::timestamptz) AS epoch,
         extract(day FROM now() - interval '1 week') AS day
`).fetchOne(pool, { at });
// parts: { month: string; epoch: string; day: string }
```

Without such a proof the same column is nullable:

```ts
const rows = await sql("SELECT to_char(created_at, 'YYYY-MM') AS month FROM users").fetchAll(pool);
// rows: { month: string | null }[]
```

### Values and ranges

The analysis knows which values a value can take when there are few — an enum's
labels, a `CHECK (kind IN (…))` list (on the table or a domain), what the
`WHERE` or a `CASE` branch leaves (`WHERE kind <> 'draft'`), literals — and
the bounds of an integer: `CHECK (n BETWEEN 1 AND 100)` or `CHECK (VALUE >
0)`, a `WHERE`, counts and positions (`count(*)`, `row_number()`, `length`),
`min` / `max` and integer arithmetic. `NULLIF(x, v)` is NOT NULL when `x`
can't be `v`, and a `CASE` covering every value a column can be has no `ELSE`
to fall to.

In TypeScript that becomes a literal union, for `text`, `varchar`, the
integer types, `boolean` and enums:

```ts
const rows = await sql("SELECT CASE WHEN visits > 10 THEN 'regular' ELSE 'new' END AS kind FROM users").fetchAll(pool);
// rows: { kind: "new" | "regular" }[]

const kinds = await sql("SELECT kind FROM kinds").fetchAll(pool);
// kinds: { kind: "a" | "b" }[]   (CHECK (kind IN ('a', 'b')))

const bands = await sql("SELECT band FROM users").fetchAll(pool);
// bands: { band: "high" | "low" | null }[]   (domain band: CHECK (VALUE IN ('low', 'high')))

const moods = await sql("SELECT mood FROM users WHERE mood <> 'neutral'").fetchAll(pool);
// moods: { mood: "happy" | "sad" }[]

const tickets = await sql("SELECT CASE status WHEN 'open' THEN 1 WHEN 'closed' THEN 2 END AS n FROM tickets").fetchAll(pool);
// tickets: { n: 1 | 2 }[]
```

Constants narrow too, and so do integer bounds and a covered `CASE`:

```ts
const row = await sql("SELECT 2::int8 AS big, 2::int4 AS small, 'hi'::text AS word, true AS yes").fetchOne(pool);
// row: { big: 2n; small: 2; word: "hi"; yes: true }

const attempts = await sql("SELECT attempts FROM jobs WHERE attempts > 0").fetchAll(pool);
// attempts: { attempts: 1 | 2 }[]   (CHECK (attempts BETWEEN 0 AND 2))

const stages = await sql(`
  SELECT total / NULLIF(row_number() OVER (), 0) AS avg,
         CASE attempts WHEN 0 THEN 'new' WHEN 1 THEN 'retried' WHEN 2 THEN 'last' END AS stage
  FROM jobs
`).fetchAll(pool);
// stages: { avg: bigint; stage: "last" | "new" | "retried" }[]
```

A parameter still takes any value of its type, and a type `types` maps keeps
its mapping.

### Nullability annotations

Override the inference where you know better: `!` forces non-NULL, `?` forces
nullable, on the alias or on the parameter name.

```ts
const rows = await sql(`
  SELECT p.title AS "title!", u.name AS "name?"
  FROM posts p JOIN users u ON u.id = p.author_id
`).fetchAll(pool);
// rows: { title: string; name: string | null }[]

await sql("UPDATE users SET age = $age! WHERE id = $id").execute(pool, { age, id });
// $age: number   (age is nullable; forced not null)

await sql("UPDATE users SET name = $name? WHERE id = $id").execute(pool, { name, id });
// $name: string | null   (name is NOT NULL; forced nullable)
```

### Enums and domains

An enum reads as the union of its labels, NULL included when the column is.
`CREATE TYPE mood AS ENUM ('happy', 'sad', 'neutral')` gives:

```ts
const rows = await sql("SELECT mood FROM users").fetchAll(pool);
// rows: { mood: "happy" | "sad" | "neutral" | null }[]
```

A domain reads as its base type, unless `types` maps it; a CHECK on the
domain can still narrow it (the `band` domain above).

```ts
const row = await sql("SELECT qty FROM users WHERE id = $id").fetchOne(pool, { id });
// row: { qty: number | null }   (qty is the domain `positive` over int4)
```

### Composites and arrays

A composite is an object with a property per field, each with its own
nullability:

```ts
const row = await sql("SELECT home, (home).street AS street FROM users WHERE id = $id").fetchOne(pool, { id });
// row: { home: { street: string | null; number: number | null } | null; street: string | null }
```

PostgreSQL has no NOT NULL for an array's elements, so a `text[] NOT NULL`
column is `(string | null)[]`; `CHECK (array_position(tags, NULL) IS NULL)` on
the column, or on a domain over the array (`VALUE`), stands for one:

```ts
const row = await sql("SELECT tags, loose_tags FROM users WHERE id = $id").fetchOne(pool, { id });
// row: { tags: (string | null)[]; loose_tags: (string | null)[] | null }

const rows = await sql("SELECT tags FROM tagged").fetchAll(pool);
// rows: { tags: string[] }[]   (CHECK (array_position(tags, NULL) IS NULL))
```

Composite values can be parameters (from their text form); an anonymous
record cannot, as PostgreSQL has no input for it. Multidimensional arrays are
an error, as in Rust.

### Mapped types

`types` maps a PostgreSQL type to a TypeScript one in the generated module. A
JSONB domain mapped to an interface reads as that interface, NULL and all:

```json
{ "types": { "user_prefs": "./src/prefs.ts#UserPrefs" } }
```

```ts
// src/prefs.ts
export interface UserPrefs {
  theme: "dark" | "light";
  fontSize?: number;
}
```

```ts
const row = await sql("SELECT prefs FROM users WHERE id = $id").fetchOne(pool, { id });
// row: { prefs: UserPrefs | null }
```

The mapping changes what the module says, not how values are read, so the type
must fit the value's decoded type — a JSONB domain takes any type, a
composite must have the fields' types, an enum a string subtype. A type
mapping also keeps its own type: it is not narrowed to a literal union.

`CREATE EXTENSION` is interpreted with the rest of the DDL, so the types,
operators and functions an extension installs are analyzed like built-ins;
`vector` / `halfvec` read as `number[]` and `hstore` as
`Record<string, string | null>`.

### Spreads and lists

`$..name { fields }` in `VALUES` is a rows spread; each field is typed by its
column, so a NOT NULL column is not nullable in the item:

```ts
await sql("INSERT INTO users (name, email) VALUES $..rows { name, email }").execute(pool, { rows });
// $rows: readonly { name: string; email: string | null }[]
```

`IN $..name` is a list; each element is typed by the `IN`'s left side:

```ts
const rows = await sql("SELECT name FROM users WHERE id IN $..ids").fetchAll(pool, { ids: [1, 2] });
// $ids: readonly number[]
```

An empty spread or list still runs as SQL would, and above 1000 items a
spread is bound as one array; the [Queries](#queries) section has both.

### Large integers

`int8` reads as `bigint`, never as a `number` that could lose precision; a
parameter also takes a `number` or a decimal `string`:

```ts
const row = await sql("SELECT id, visits FROM users WHERE id = $id").fetchOne(pool, { id });
// row: { id: number; visits: bigint }

await sql("UPDATE users SET visits = visits + $by WHERE id = $id").execute(pool, { by, id });
// $by: bigint | number | string
```

## Two ways to write queries

- **Inline**, in a `sql("…")` call in your sources, typed by the generated
  module named by the database's `out`.
- **In a `.sql` file**, one `/* @name */` block per query, listed by
  `sql.include` and written by `gen` as `emit` says.

### Generate and build

```sh
npx typedpg gen            # write the generated modules
npx typedpg gen --watch    # and keep them up to date
npx typedpg check          # in CI: fail if a module is stale or a query has an error
npx typedpg build          # compile the .sql files into the loaders' cache
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
when its content changes.

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

The SQL can be anything PostgreSQL accepts: CTEs, window functions,
`LATERAL`, `RETURNING`, `ON CONFLICT`, `DISTINCT ON`…

- **Inference**: columns, joins, aggregates, conditions, literal unions and
  annotations — see [What typedpg infers](#what-typedpg-infers).
- **Bulk insert**: `INSERT INTO users (name, email) VALUES $..rows { name, email }`,
  with `rows` an array of objects, each field typed by its column. With no row
  the query still runs, as SQL would: an empty spread next to other rows is
  left out, and a `VALUES` of nothing but empty spreads is written as a
  `SELECT` of no row (PostgreSQL has no `VALUES` without one).
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

## Queries in .sql files

Each query is a `/* @name */` comment and one statement, with the same
`$name` parameters, `$..spreads` and `"col!"` annotations:

```sql
-- src/users/queries.sql
/* @name userById */
SELECT id, name, email FROM users WHERE id = $id;

/* @name renameUser */
UPDATE users SET name = $name WHERE id = $id;
```

```json
{ "sql": { "include": ["src/**/queries.sql"] } }
```

Each query is an export, typed with an interface per query
(`UserByIdParams`, `UserByIdRow`):

```ts
import { userById, type UserByIdRow } from "./users/queries.ts";

const user = await userById.fetchOne(pool, { id: 42 });
```

`include` takes globs relative to the config's directory (`*` stays in a
directory, `**` crosses them; the migrations directories are never read).
An error points into the file (`src/users/queries.sql:2:8: error: …`), and
a query with one is still exported, so each use of it fails to type-check
with the error. `typeNames` names the interfaces: `{ "params":
"{Name}Params", "row": "{Name}Row" }` by default, `{Name}` the query's name
with its first letter capitalized — `"I{Name}Params"` / `"I{Name}Result"`
are pgtyped's.

What `typedpg gen` writes for each file is `emit`:

| `emit` | Writes | Code imports | Types for |
|---|---|---|---|
| `"ts"` (default) | the module beside it, at `out` (default `"{dir}/{name}.ts"`; `.js`, `.mjs`, `.cjs` work as for the database's `out`) | `./queries.ts` | any tsc, `bun check` |
| `"dts"` | `queries.d.sql.ts`, the types alone | `./queries.sql` (a loader runs it) | any tsc with `allowArbitraryExtensions`, `bun check` |
| `"none"` | nothing | `./queries.sql` (a loader runs it) | tsc 7.1+, through the content mapper |

`gen` won't overwrite a file it didn't generate (but pgtyped's, to ease
moving from it), and deletes the module of a `.sql` file that is gone.

### Importing .sql files

With `emit` `"dts"` or `"none"`, the code imports the `.sql` file itself,
and a loader or bundler plugin gives its module:

| Runtime or bundler | |
|---|---|
| Node.js 22.15+ | `node --import @cubos/typedpg/register app.ts` |
| Bun | `bun --preload @cubos/typedpg/bun app.ts` (or `preload` in `bunfig.toml`); `Bun.build({ plugins: [typedpg] })` with `import typedpg from "@cubos/typedpg/bun"` |
| esbuild | `plugins: [typedpg()]`, `import typedpg from "@cubos/typedpg-cli/esbuild"` |
| Vite, vitest, Rollup, Rolldown | `plugins: [typedpg()]`, `import typedpg from "@cubos/typedpg-cli/vite"` (or `/rollup`) |
| webpack, rspack | `{ test: /\.sql$/, loader: "@cubos/typedpg-cli/webpack", type: "javascript/esm" }` |

The runtime ships the Node and Bun loaders, which run in production; the
bundler plugins are in `@cubos/typedpg-cli`, a devDependency. The loaders
and plugins do not analyze queries themselves: they read the module from a
cache, `node_modules/.cache/typedpg` (`TYPEDPG_CACHE_DIR` to move it), keyed
by the typedpg version, the config, the migrations and the file. On a miss, when
the CLI is installed, as it is in development, the bundler plugins and Bun's
loader compile through a persistent `typedpg serve` — one child per binary,
which keeps the schema in memory between edits and exits after five minutes
without a request (`TYPEDPG_SERVE_IDLE_TIMEOUT`, in seconds); the next miss
spawns another. (The Node hook is synchronous and runs `typedpg build`
once, which compiles every file missing an entry.) Run `typedpg build` when
building an image: the cache is then complete, and the image needs neither
the binary nor the analysis at startup. A production install has no binary,
so a cache miss there is an error that says which file and cache directory,
and to run `typedpg build` where the project is built.

### The content mapper (TypeScript 7.1)

TypeScript 7.1 reads a `.sql` file's types from `@cubos/typedpg-cli` — the
`typedpg serve` command TypeScript runs — with no file written (`emit:
"none"`), so the CLI must be a devDependency:

```json
{
  "contentMappers": [{ "package": "@cubos/typedpg-cli", "extensions": [".sql"] }]
}
```

`tsc --runExternalCode` then checks the `.sql` files too, each error at its
place in the file. In VS Code it needs a trusted workspace, and the
[typedpg extension](https://github.com/cubos/typedpg/blob/main/typedpg_vscode/README.md)
for the errors inside an open `.sql` file (the hover and types where it is
imported work without it). The `typedpg.config.json` is the nearest one
above `tsconfig.json`, or the mapper's `"options": { "config":
"path/to/typedpg.config.json" }`. With a `.d.sql.ts` present too, the mapper
wins: commit them (`emit: "dts"`) for the tools without content mappers, and
tsc 7.1 still reads the `.sql` file. A new migration file is seen once the
config or a known migration changes (TypeScript watches the files the mapper
names, not directories).

## In production

`@cubos/typedpg` holds no native binary: a `--omit=dev` install has no
`typedpg` command. The Node and Bun loaders run from the cache `typedpg
build` fills (see [Importing .sql files](#importing-sql-files)). The Node
hook:

```sh
node --import @cubos/typedpg/register app.js
```

Migrations run at startup with `migrate()`, from the generated module
(`"migrations": { "embed": true }`) or from a directory with
`loadMigrations()`; or in a deploy step with `npx typedpg migrate up`, which
needs `@cubos/typedpg-cli` and a `DATABASE_URL` in that step.

## Executors and drivers

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
| `bool` | `boolean`; a parameter must be a boolean (`true` / `false`) — a string or a number is refused |
| `int2`, `int4`, `float4`, `float8`, `oid` | `number` |
| `int8` | `bigint`; parameters also take `number` and `string` |
| `numeric` | `string` (exact); parameters also take `number` |
| `text`, `varchar`, `char`, `uuid`, `date`, `time`, `timetz`, `inet`, `money`, … | `string` |
| `timestamptz`, `timestamp` | `Date`, parameters too (a `timestamp`, which has no time zone, is read and written as UTC). A `Date` is never `infinity`: `to_char` and `extract` of a parameter are never NULL |
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

A value the query proves holds one of a few values reads as their literal
union — see [Values and ranges](#values-and-ranges).

In a `json`/`jsonb` parameter, `null` is SQL NULL, not the JSON `null`.

### Mapping types

A mapped type must fit how its values are decoded (see [Types](#types)): a
narrower one is fine — a branded `string`, a string `enum` for an enum, an
interface for a composite, any type for JSON — a different one is a
TypeScript error in the generated module. Mapping a `halfvec` to `string`:

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
npx typedpg migrate accept <name>      # record an intentional edit; runs nothing
npx typedpg migrate up --db warehouse  # with several databases
```

Editing an applied migration's statements is drift: with `failOnDrift` (the
default) the run aborts before applying anything new and prints a unified diff
of the applied text against the file; with `failOnDrift: false` it only warns.
Comments, whitespace and the case of unquoted words do not count. An
intentional edit that does not need to run again is recorded with
`acceptMigration` (or `typedpg migrate accept <name>`) without running it.

At startup, from the generated module (`"migrations": { "embed": true }`)
or from a directory:

```ts
import { acceptMigration, migrate, migrationStatus, revertMigration, loadMigrations } from "@cubos/typedpg";
import { migrations } from "./db"; // or: loadMigrations("./migrations")

const applied = await migrate(pool, migrations);
await acceptMigration(pool, migrations, "0002_add_email"); // after an intentional edit
```

Pass a pool or a client, not a transaction: each migration runs in its own.

## Configuration reference

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
| `out` | The generated module of the queries in sources. `.ts`/`.mts`/`.cts` generates TypeScript; `.js`/`.mjs` an ES module and its `.d.ts`; `.cjs` a CommonJS module and its `.d.cts`. |
| `sql` | The `.sql` files of queries: `{ "include", "emit", "out", "typeNames" }`, see [Queries in .sql files](#queries-in-sql-files). A database has `out`, `sql` or both. |
| `migrations` | The migrations directory (default `migrations`), or `{ "dir", "table", "lockId", "useTransaction", "failOnDrift", "embed" }` — the runner's settings, as in Rust. `"embed": true` adds the migrations to the generated module. |
| `extraMigrations` | More directories the schema is built from but the runner doesn't apply (tables another project owns). |
| `types` | PG type → TypeScript type, as `"./src/types.ts#Prefs"` (relative to the config) or `"package#Type"`. A JSONB domain, an enum, a composite or any other type; an unqualified name is in `public`, a built-in type is `pg_catalog.<name>`. See [Mapping types](#mapping-types). |
| `runtime` | The module the generated code imports the runtime from (default `@cubos/typedpg`). |
| `databases` | Several databases: `{ "main": { "out": …, "migrations": … }, "warehouse": { … } }`, each with the keys above. A query belongs to the database whose module its `sql` comes from. |

Unknown keys are errors.

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

## Upgrading to 0.6.0

The `typedpg` command and the bundler plugins moved out of `@cubos/typedpg`
into `@cubos/typedpg-cli`:

- add it as a devDependency: `npm install --save-dev @cubos/typedpg-cli`;
- import the bundler plugins from `@cubos/typedpg-cli/esbuild`, `/vite`,
  `/rollup` or `/webpack` — the old `@cubos/typedpg/<plugin>` subpaths still
  resolve, but throw an error saying where they moved;
- `npx typedpg` comes from `@cubos/typedpg-cli`;
- a production install of `@cubos/typedpg` no longer carries the binary: run
  `typedpg build` when building the image (see
  [Importing .sql files](#importing-sql-files)).

Drift detection changed too: comments, whitespace and the case of unquoted
words no longer count as drift, and an edit can be recorded with `typedpg
migrate accept <name>` without running it. On the first run, the tracking
table gains `hash` and `hash_version` columns automatically; a 0.5 runner
on the same database ignores them.

## License

MIT.
