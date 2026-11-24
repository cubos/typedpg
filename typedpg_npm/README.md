# typedpg for TypeScript

Typed PostgreSQL queries for TypeScript, checked against your migrations —
no database needed at build time. Write plain SQL; `typedpg gen` analyzes
every query with the same engine as [typedpg for Rust](https://github.com/cubos/typedpg)
(PostgreSQL 19's own parser, an in-memory catalog built from your
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
…), so `npm install --prod` never downloads it. Node.js 26 or later, whose
`Temporal` dates, times and durations are what date and time columns read
as, and TypeScript 6.0 or later for its declarations of them (the runtime's
types reference its `esnext.temporal` lib, so no `lib` setting is needed).

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
`NOT ENFORCED` says nothing). Under that assumption, a `LEFT JOIN` along a
NOT NULL foreign key always finds its row, and so does a scalar lookup of it:

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
// tasks: { done_at: Temporal.Instant }[]
```

Besides strict conditions, a condition that can't be true with a column NULL
proves that column non-NULL: `coalesce(age, 0) > 0`, `age IS NOT DISTINCT
FROM 5`, `(tenant_id, id) = ($1, $2)`, or an `EXISTS` whose subquery's
`WHERE` is strict in an outer column. A condition that is itself never NULL,
such as `b IS NULL OR b > 0`, is tracked too, where that matters. Conditions
that may hold for a NULL prove nothing: `coalesce(age, 1) > 0`, `age IS
DISTINCT FROM 5`, or an `OR` whose arms test different columns.

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
literal other than `infinity`, a `date` / `timestamp` / `timestamptz`
parameter (a Temporal value cannot hold an infinity — unless `types` maps
the type), arithmetic,
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
`WHERE` or a `CASE` branch leaves (`WHERE kind <> 'draft'`, `WHERE kind =
ANY ('{a,b}')`, `= ANY` of an array whose elements are known), literals — and
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

An enum is a type the generated module exports, the union of its labels as
declared, NULL added when the column is nullable. `CREATE TYPE mood AS ENUM
('happy', 'sad', 'neutral')` gives `export type Mood = "happy" | "sad" |
"neutral";` in the module, for your own code to use too:

```ts
import { sql, type Mood } from "./db.ts";

const rows = await sql("SELECT mood FROM users").fetchAll(pool);
// rows: { mood: Mood | null }[]

const cheerful = (mood: Mood) => sql("SELECT name FROM users WHERE mood = $mood").fetchAll(pool, { mood });
```

The name is the type's in PascalCase, the schema's before it outside
`public` (`billing.status` is `BillingStatus`). A name the module already
has (`Queries`, a type `types` maps to, a `.sql` file's query types) adds the
schema's (`PublicQueries`), then a number (`BillingStatus_2`). The database's
module exports every enum of the schema, and an extension's only when a
query uses it; a `.sql` file's module exports the enums its queries' types
use. An enum `types` maps is not exported: the mapping wins. A value
narrowed to some labels (`WHERE mood <> 'neutral'`) stays their union,
`"happy" | "sad"`.

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

An empty spread or list still runs as SQL would; above 1000 items (or fewer,
when their placeholders would pass 65535 parameters) a list is
bound as one array and a rows spread as one array per field. The
[Queries](#queries) section has both.

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
npx typedpg lint           # run the lint rules over every query and migration
npx typedpg lint --fix     # apply the findings' SQL fixes, then lint again
npx typedpg build          # compile the .sql files into the loaders' cache
```

`lint` runs these rules over the queries — analyzed as the database's
`roles`, as `typedpg check` analyzes them, the first role's analysis read by
the rules — and the database's migrations. Each rule has a page in
[`docs/lint`](https://github.com/cubos/typedpg/blob/main/docs/lint/README.md)
with its motivation, what it proves, examples and configuration:

| Rule | Default | What it reports |
|---|---|---|
| [`null_comparison`](https://github.com/cubos/typedpg/blob/main/docs/lint/null_comparison.md) | error | A comparison with a `NULL` literal, which is never true. |
| [`unbounded_write`](https://github.com/cubos/typedpg/blob/main/docs/lint/unbounded_write.md) | error | An `UPDATE` or `DELETE` with no `WHERE`. |
| [`not_in_nullable`](https://github.com/cubos/typedpg/blob/main/docs/lint/not_in_nullable.md) | warning | `NOT IN (SELECT …)` where either side may be NULL, which returns no rows. |
| [`create_index_blocking`](https://github.com/cubos/typedpg/blob/main/docs/lint/create_index_blocking.md) | warning | `CREATE INDEX` without `CONCURRENTLY` on a table that already has rows. |
| [`constraint_validating`](https://github.com/cubos/typedpg/blob/main/docs/lint/constraint_validating.md) | warning | `ADD CONSTRAINT` / `ADD COLUMN` validating every row under a lock. |
| [`set_not_null_scan`](https://github.com/cubos/typedpg/blob/main/docs/lint/set_not_null_scan.md) | warning | `ALTER COLUMN … SET NOT NULL` scanning the table. |
| [`table_rewrite`](https://github.com/cubos/typedpg/blob/main/docs/lint/table_rewrite.md) | warning | `ALTER TABLE` rewriting every row under an ACCESS EXCLUSIVE lock. |
| [`fk_without_index`](https://github.com/cubos/typedpg/blob/main/docs/lint/fk_without_index.md) | warning | A foreign key whose referencing columns have no index. A partial index over exactly those columns `WHERE col IS NOT NULL` counts: the foreign-key lookup's equality implies the predicate. |
| [`redundant_index`](https://github.com/cubos/typedpg/blob/main/docs/lint/redundant_index.md) | info | An index another index on the table already covers. |
| [`redundant_null_check`](https://github.com/cubos/typedpg/blob/main/docs/lint/redundant_null_check.md) | warning | `IS [NOT] NULL` or `COALESCE` on a value that is never NULL. |
| [`limit_without_order`](https://github.com/cubos/typedpg/blob/main/docs/lint/limit_without_order.md) | warning | `LIMIT`, `OFFSET` or `DISTINCT ON` without `ORDER BY`, which picks arbitrary rows. |
| [`redundant_limit`](https://github.com/cubos/typedpg/blob/main/docs/lint/redundant_limit.md) | info | A `LIMIT` on a query that returns at most one row. |
| [`redundant_distinct`](https://github.com/cubos/typedpg/blob/main/docs/lint/redundant_distinct.md) | info | A `DISTINCT` on rows that are already distinct. |
| [`constant_condition`](https://github.com/cubos/typedpg/blob/main/docs/lint/constant_condition.md) | warning | A `WHERE`, `JOIN … ON`, `HAVING` or `FILTER` condition — or an `AND` / `OR` / `NOT` part of one, or of a `CASE WHEN` — that is always true or never true: one the planner folds to a constant (`1 = 1`), or one what the analyzer knows decides — a column's `CHECK` constraints, domain or enum and the conditions before it (`status = 'actve'` when `status` is `CHECK (status IN ('active', 'inactive'))`, `n < 0` under `CHECK (n >= 0)`, `a = 2` after `a = 1 AND`, `x = x` on a column never NULL, an `EXISTS` whose subquery always returns a row). The message says why; a column of a table with a `NOT VALID` `CHECK` is not judged. The fix removes a `WHERE` the planner folds to true from a `SELECT`. |
| [`unreachable_case_branch`](https://github.com/cubos/typedpg/blob/main/docs/lint/unreachable_case_branch.md) | warning | A `CASE` branch that is never taken: a `WHEN` that is never true, or what follows one that always is — folded by the planner, or decided by what the analyzer knows there (the constraints, the conditions around the `CASE`, the `WHEN`s before it). |
| [`case_without_else`](https://github.com/cubos/typedpg/blob/main/docs/lint/case_without_else.md) | warning | A `CASE` without `ELSE` whose `WHEN`s may all fail, so it is NULL then; a `CASE` the constraints prove exhaustive is not reported. The fix writes `ELSE NULL`. |
| [`redundant_cast`](https://github.com/cubos/typedpg/blob/main/docs/lint/redundant_cast.md) | info | A cast to the type (and type modifier) its operand already has. |
| [`redundant_alias`](https://github.com/cubos/typedpg/blob/main/docs/lint/redundant_alias.md) | info | An output column alias that is the name the column already has (`SELECT name AS name`). |
| [`outer_reference_in_subquery`](https://github.com/cubos/typedpg/blob/main/docs/lint/outer_reference_in_subquery.md) | warning | An unqualified column in a subquery that no relation of the subquery has, so it is the outer query's (`WHERE id IN (SELECT id FROM b)` with no `b.id`). |
| [`cartesian_join`](https://github.com/cubos/typedpg/blob/main/docs/lint/cartesian_join.md) | warning | FROM items no `WHERE` / `ON` condition connects, directly or through other items, so every row pairs with every row; an explicit `CROSS JOIN`, a function in FROM, a `LATERAL` item referencing its left and an item of at most one row are not reported. |
| [`insert_without_columns`](https://github.com/cubos/typedpg/blob/main/docs/lint/insert_without_columns.md) | warning | An `INSERT INTO t VALUES …` / `SELECT …` without a column list, which depends on the table's column order. The fix writes the columns it targets now. |
| [`self_assignment`](https://github.com/cubos/typedpg/blob/main/docs/lint/self_assignment.md) | warning | An `UPDATE … SET a = a`, which changes nothing. The fix removes it when another assignment remains and no `UPDATE OF a` trigger fires on it. |
| [`unused_cte`](https://github.com/cubos/typedpg/blob/main/docs/lint/unused_cte.md) | warning | A `WITH` query (not a data-modifying one) nothing references. The fix removes it. |
| [`simplifiable_case`](https://github.com/cubos/typedpg/blob/main/docs/lint/simplifiable_case.md) | info | `CASE WHEN x IS NULL THEN y ELSE x END`, which is `COALESCE(x, y)`, and `CASE WHEN c THEN true ELSE false END`, which is `c` when `c` is never NULL. The fix rewrites it. |
| [`like_unescaped_param`](https://github.com/cubos/typedpg/blob/main/docs/lint/like_unescaped_param.md) | off | A `LIKE` pattern built from a parameter whose `%` and `_` are not escaped. |
| [`redundant_join`](https://github.com/cubos/typedpg/blob/main/docs/lint/redundant_join.md) | info | A join that can't change the rows and whose table nothing else reads: a `LEFT JOIN` whose `ON` equates every column of a full unique key of the joined table with columns of the same type (each row matches at most one row), or an inner `JOIN` over exactly a NOT NULL, validated, `NOT DEFERRABLE` foreign key into a parent without row-level security (each row matches exactly one). Only a plain `SELECT`'s joins to a table are read; a join whose removal would change a column's refinement is kept. The fix deletes the join. |
| [`scalar_subquery_may_return_many`](https://github.com/cubos/typedpg/blob/main/docs/lint/scalar_subquery_may_return_many.md) | warning | A subquery used as a value (`(SELECT x FROM t WHERE …)` in a select list, a comparison, a `SET`, a function argument) that may return more than one row, which fails at run time with "more than one row returned by a subquery used as an expression". A subquery proven to return at most one row (an aggregate without `GROUP BY`, a unique key pinned by its `WHERE`, outer references included, `LIMIT 1`) is not reported, nor are `IN`, `EXISTS`, `ANY` / `ALL` and `ARRAY(…)` subqueries, nor a subquery over a view, a CTE, a function, a FROM subquery or a table with an expression or partial unique index, which the proof can't see through. |
| [`timestamp_timezone_mix`](https://github.com/cubos/typedpg/blob/main/docs/lint/timestamp_timezone_mix.md) | warning | An implicit conversion between `timestamp` (or `date`) and `timestamptz` — a comparison (`created_at > now()` over a `timestamp` column), arithmetic, a function argument, a `CASE` / `COALESCE` / `UNION` type resolution, a value assigned to a column of the other type — whose result depends on the session's `TimeZone`. An explicit cast or `AT TIME ZONE`, a parameter and an untyped literal are not reported. |
| [`integer_division`](https://github.com/cubos/typedpg/blob/main/docs/lint/integer_division.md) | warning | An integer division, which truncates, whose result is then used as a `numeric` or floating-point value: `(done / total)::numeric`, `done / total * 100.0`, `done / total > 0.5`, `round(done / total, 2)`, or an integer product of it so used (`(done / total * 100)::numeric`). Convert an operand first: `done::numeric / total`. |
| [`left_join_nullified`](https://github.com/cubos/typedpg/blob/main/docs/lint/left_join_nullified.md) | warning | A `WHERE` condition never true for the null-extended rows of an outer join's nullable side (`LEFT JOIN b … WHERE b.status = 'x'`, `b.id IS NOT NULL`), so the join behaves as an inner join; `b.id IS NULL` (the anti-join), non-strict tests like `COALESCE(b.x, 0) = 0`, an `OR` with an arm keeping the rows and a join that never null-extends (a foreign key's parent) are not reported. |
| [`check_violation`](https://github.com/cubos/typedpg/blob/main/docs/lint/check_violation.md) | warning | An `INSERT … VALUES`, `UPDATE … SET` or `ON CONFLICT DO UPDATE SET` storing a constant a `CHECK` constraint (`NOT VALID` ones included) or the column's domain rejects whatever the rest of the row, so the statement fails for every row it writes; not reported when a `BEFORE` row trigger or a rule may change the row. |
| [`unstable_order`](https://github.com/cubos/typedpg/blob/main/docs/lint/unstable_order.md) | warning | An `ORDER BY` under `LIMIT`, `OFFSET` / `FETCH` or `DISTINCT ON` whose keys don't determine a unique row — no unique key (NOT NULL, or `NULLS NOT DISTINCT`) of every table is covered, counting columns `WHERE` pins to a constant, tables joined on their key and grouping keys — so tied rows come back in an arbitrary order and pages may skip or repeat rows. The help names the primary key to add as a tiebreaker. Queries returning at most one row, `WITH TIES`, `SKIP LOCKED` queues and FROM clauses the proof can't see through (subqueries, CTEs, partial or expression unique indexes) are not reported. |
| [`redundant_order_by`](https://github.com/cubos/typedpg/blob/main/docs/lint/redundant_order_by.md) | info | `ORDER BY` keys that can't change the order: a key after keys that already determine a unique row (the proof `unstable_order` uses), a column `WHERE` pins to a constant or parameter (`WHERE status = 'a' ORDER BY status, id`), a column an earlier key already sorts by, and the whole `ORDER BY` of a query returning at most one row or of an `EXISTS` / `IN` / `ANY` / `ALL` subquery (without `LIMIT`, `OFFSET`, `DISTINCT ON` or a locking clause), whose order nothing observes. `FROM` subqueries, CTEs and `ARRAY(…)` are not judged, nor a `DISTINCT ON`'s leading keys, nor a last key under `LIMIT` / `OFFSET` (that is `unstable_order`'s). The fix removes plain-column, output-name and ordinal keys (the whole clause when all go). |
| [`redundant_group_by`](https://github.com/cubos/typedpg/blob/main/docs/lint/redundant_group_by.md) | info | `GROUP BY` keys that can't split a group: a column of a table whose other grouped columns cover a unique NOT NULL key of it, or one `WHERE` pins to a constant or parameter. A key the select list, `HAVING`, `ORDER BY` or a window reads is only reported when the table's primary key stays grouped (PostgreSQL accepts nothing else for an ungrouped column), one key always stays, and `GROUPING SETS` / `ROLLUP` / `CUBE` are never touched. The fix removes the keys. |
| [`count_not_null`](https://github.com/cubos/typedpg/blob/main/docs/lint/count_not_null.md) | info | `count(expr)` over a value never NULL — a NOT NULL column no outer join null-extends, a literal (`count(1)`), a column the call's `FILTER` proves non-NULL — which counts every row like `count(*)`. `DISTINCT`, row values and parameters are not reported, nor an aggregate of outer columns (evaluated in the outer query). The fix writes `count(*)` when the argument is a plain column or constant. |
| [`non_sargable_predicate`](https://github.com/cubos/typedpg/blob/main/docs/lint/non_sargable_predicate.md) | info | A `WHERE` / `JOIN … ON` comparison (`=`, `<`, `<=`, `>`, `>=`, `BETWEEN`, `IN`, `= ANY`) that wraps an indexed column so no index can serve it: a converting cast (`created_at::date = $1`, `id::text = $1`), a function (`lower(email) = $1`, `date_trunc('day', ts) = $1`), arithmetic (`id + 1 = $1`) or an implicit conversion the other side's type forces (an `int4` column compared with a `numeric` value). Reported only when the column leads a non-partial btree index and its table has no expression index; the help names the range rewrite for `::date` and `date_trunc`. |
| [`float_equality`](https://github.com/cubos/typedpg/blob/main/docs/lint/float_equality.md) | warning | An `=`, `<>`, `IN` or `NOT IN` comparison made between `real` / `double precision` values (after implicit conversions), which rarely holds for computed values (`0.1 + 0.2 = 0.3` is false). A comparison with a value exact in binary floating point — an integer or decimal literal the type holds exactly (`0`, `0.5`, `1e3`; up to 2^24 for `real`), `'NaN'`, `'Infinity'` — is not reported; columns, parameters and joins (`a.f = b.f`) are. |
| [`fk_type_mismatch`](https://github.com/cubos/typedpg/blob/main/docs/lint/fk_type_mismatch.md) | warning | A foreign key whose column types differ from the referenced columns. |
| [`missing_lock_timeout`](https://github.com/cubos/typedpg/blob/main/docs/lint/missing_lock_timeout.md) | warning | A migration taking a blocking lock on an existing table without `SET lock_timeout`. |
| [`drop_index_blocking`](https://github.com/cubos/typedpg/blob/main/docs/lint/drop_index_blocking.md) | warning | `DROP INDEX` without `CONCURRENTLY` on an existing table, which takes an ACCESS EXCLUSIVE lock on it. |
| [`blocking_rebuild`](https://github.com/cubos/typedpg/blob/main/docs/lint/blocking_rebuild.md) | warning | `REINDEX` without `CONCURRENTLY`, `VACUUM FULL`, `CLUSTER` or `REPACK` without `CONCURRENTLY` on an existing table, which hold their lock for the whole rebuild. |
| [`detach_partition_blocking`](https://github.com/cubos/typedpg/blob/main/docs/lint/detach_partition_blocking.md) | warning | `DETACH PARTITION` without `CONCURRENTLY` on an existing partitioned table. |
| [`validate_in_same_transaction`](https://github.com/cubos/typedpg/blob/main/docs/lint/validate_in_same_transaction.md) | warning | `VALIDATE CONSTRAINT` in the transaction that added the constraint `NOT VALID`, while `ADD CONSTRAINT`'s lock is still held. |
| [`attach_partition_scan`](https://github.com/cubos/typedpg/blob/main/docs/lint/attach_partition_scan.md) | warning | `ATTACH PARTITION` of an existing table whose validated `CHECK` and `NOT NULL` constraints don't imply the partition bound (proven as PostgreSQL proves it; a `CHECK` alone doesn't make the key `NOT NULL`), so PostgreSQL scans it under lock to check the bound; also the scan of an existing default partition. |
| [`missing_primary_key`](https://github.com/cubos/typedpg/blob/main/docs/lint/missing_primary_key.md) | warning | A table without a primary key; partitions (which get their parent's) and temporary tables are skipped. |
| [`rls_policy_not_enforced`](https://github.com/cubos/typedpg/blob/main/docs/lint/rls_policy_not_enforced.md) | error | Row-level security policies on a table whose row-level security is not enabled: they do nothing. |
| [`rls_no_policy`](https://github.com/cubos/typedpg/blob/main/docs/lint/rls_no_policy.md) | info | Row-level security enabled on a table with no policy, which denies every role but the owner, superusers and `BYPASSRLS` roles. |
| [`security_definer_search_path`](https://github.com/cubos/typedpg/blob/main/docs/lint/security_definer_search_path.md) | warning | A `SECURITY DEFINER` function or procedure without `SET search_path`, which resolves names along the caller's search path; a SQL-standard (`BEGIN ATOMIC`) body, resolved at creation, is not reported. |
| [`prefer_timestamptz`](https://github.com/cubos/typedpg/blob/main/docs/lint/prefer_timestamptz.md) | off | A `timestamp without time zone` column. |
| [`prefer_identity`](https://github.com/cubos/typedpg/blob/main/docs/lint/prefer_identity.md) | off | A `serial` column instead of an identity column. |
| [`prefer_text`](https://github.com/cubos/typedpg/blob/main/docs/lint/prefer_text.md) | off | A `varchar(n)` or `char(n)` column instead of `text`. |
| [`prefer_bigint_pk`](https://github.com/cubos/typedpg/blob/main/docs/lint/prefer_bigint_pk.md) | off | An `int2` or `int4` primary key. |
| [`prefer_jsonb`](https://github.com/cubos/typedpg/blob/main/docs/lint/prefer_jsonb.md) | off | A `json` column, which has no equality operator (`DISTINCT`, `GROUP BY`, `UNION` fail), instead of `jsonb`. |
| [`fetch_may_return_many`](https://github.com/cubos/typedpg/blob/main/docs/lint/fetch_may_return_many.md) | warning | `fetchOne` / `fetchValue` / `fetchOptional` / `fetchValueOptional` on a query not proven to return at most one row. |
| [`fetch_all_single_row`](https://github.com/cubos/typedpg/blob/main/docs/lint/fetch_all_single_row.md) | info | `fetchAll` on a query that returns at most one row. |
| [`unused_returning`](https://github.com/cubos/typedpg/blob/main/docs/lint/unused_returning.md) | info | `execute` on a statement whose `RETURNING` rows are discarded. |
| [`json_stringify_param`](https://github.com/cubos/typedpg/blob/main/docs/lint/json_stringify_param.md) | error | `JSON.stringify(…)` passed as a `json` / `jsonb` parameter's value (in the parameters object, a `jsonb[]` array, a rows spread's item or a `copyIn` row), which stores a JSON string instead of the value: pass the value itself. |
| [`breaks_deployed_queries`](https://github.com/cubos/typedpg/blob/main/docs/lint/breaks_deployed_queries.md) | warning | With `--previous-deployment <ref>` only: a migration that breaks a query of the code at that ref, which keeps running while the migration is applied. See below. |

A call-site finding is placed at the method identifier in the calling file,
which may be a different file from the `.sql` query. The fetch checks
(`fetch_may_return_many`, `fetch_all_single_row`) see calls directly on
`sql(…)`, through a local `const`/`let` binding, and through an exported `.sql`
query or an exported TypeScript `sql(…)` binding imported by name, alias or
namespace, and judge each call on its own. `unused_returning` needs every use
of the query, so it only reports an inline `sql(…).execute(…)` or a local
binding whose every use is an `execute`: a `.sql` query and an exported
`sql(…)` binding can be called from anywhere, so they are never reported, nor
is a binding passed to a function. Its fix deletes the `RETURNING` clause from
that inline SQL.

The `lint` object sets each rule's level — `"error"`, `"warning"`, `"info"`
or `"off"` — and `failOn`, the severity the run fails on (default `"error"`).
`ignore` lists glob patterns relative to the config file's directory, with
forward slashes:

```json
"lint": { "failOn": "warning", "ignore": ["tests/**"] }
```

Findings in a matching query file are not reported. A matching migration file
is still applied and its schema rules still report — on the statement that
created the object — but its statement rules are skipped: the fix belongs in a
new migration. Failures are never dropped: a query in an ignored file that
does not compile still fails lint. A comment suppresses a rule for the query
or migration statement it is in: `-- typedpg-lint: allow(unbounded_write)`
(the `/* … */` form works too).
Migration rules only apply to tables that existed before the file started;
schema rules are placed at the statement that created the index or constraint.
Findings go to stdout, the summary to stderr, and the run exits non-zero when
a finding reaches `failOn` or a query or migration fails analysis.
`--previous-deployment <ref>` names the git ref of what is deployed now (for
example `origin/main`; `--ignore-migrations-from` is its old name). It skips
the statement rules of migration files identical to their version at that git
ref: they build the schema, but their locks, scans and rewrites are not
reported. The schema rules still report the current schema's problems, placed
on the statement that created the object, with a help line that puts the fix
in a new migration; their allow comments are honored and validated. A file
absent from the ref, or changed by the branch, is linted as usual.
It also runs `breaks_deployed_queries`: the code at the ref keeps running
while the new migrations are applied and during the rollout, so its queries
(read from the ref, even those the branch changed or removed) are analyzed
against the new schema, and each one that breaks is reported on the statement
that breaks it, with the query's file and line at the ref — it no longer
analyzes, a result column is gone, changes type or may now be NULL, the column
list changes, a parameter changes type or no longer accepts NULL, an `INSERT
… SELECT` leaves out a new `NOT NULL` column without a default, or a result
may now be a value the code doesn't know (a new enum label, a wider `CHECK`).
An `INSERT`, `UPDATE` or `MERGE` breaks when a column it writes accepts fewer
values than the value it writes may be: a shorter `varchar(n)`, a `numeric`
with fewer digits (a value fails, or is rounded when it has fewer after the
decimal point), a new `NOT NULL`, a new or narrower `CHECK` of the table or
the column's domain (`NOT VALID` ones too). A parameter may be any value; a
constant, a `CASE` of constants or a column whose own type or `CHECK`s keep
it in range is reported only when it may not fit. A `CHECK` conjunct the
analyzer can't read of one column (`length(name) > 1`) may reject the rows
writing the columns it reads; constraints are compared by what they check,
not their name. A new `UNIQUE` constraint is not reported. Each query is
analyzed as the database's `roles`, so a revoked privilege, or row-level
security that denies a role the rows (no policy for it, or only ones that are
never true), is reported too.
Deploy code that works with both schemas first, then the migration
(expand/contract).
`--fix` writes the SQL fixes findings carry (the ones that have them; migration
findings never do) into the sources and `.sql` files, then lints again and
reports what remains. A finding's edits are applied all or none, one
overlapping an edit already accepted is skipped, and a file that changed while
linting is not fixed.
`--format json` prints one JSON document (findings, failures and the summary)
and `--format sarif` prints SARIF 2.1.0 for code scanning; both include the
analysis failures, which the human format keeps on stderr.

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
- **Large spreads**: above 1000 items (or fewer, when their placeholders
  would pass 65535 parameters), a list is bound as one array —
  `(id = ANY($1::type[]))` — and a `VALUES` of one rows spread as `SELECT *
  FROM unnest($1::type1[], …)`, a parameter per field: faster, and past
  PostgreSQL's 65535-parameter limit. Only where the analyzer proves it the
  same query (same columns, types and nullability); otherwise, as written.
- **Simplifications**: where the analysis proves an equivalent query returns
  the same rows in every state the schema allows, the one PostgreSQL plans
  better is sent — a `NOT IN` as an anti-join, a count comparison as
  `EXISTS`, a redundant `DISTINCT`, a `UNION` as `UNION ALL`, a join a foreign
  key proves redundant — so the SQL in the generated module's doc comment may
  differ from the source. On by default; the configuration's `simplify` turns
  them all off, or one rule. Queries with a `$..spread` are sent as written.
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
| Node.js | `node --import @cubos/typedpg/register app.ts` |
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
| postgres.js `sql`, a transaction's `sql`, a reserved connection | Versions 3.4 and 4. |
| a `Driver` of your own | Implement `typedpgQuery(text, params)` (and the optional methods for streams, COPY and migrations). |

Transactions are the driver's: run queries on the `PoolClient` inside your
`BEGIN`/`COMMIT`, or on postgres.js's `sql.begin(async (tx) => …)`.

Every value is read in PostgreSQL's text format with the driver's own type
parsing off, and decoded by typedpg: the types are what the generated module
says, whatever `pg.types.setTypeParser` your application set up.

A query is sent on the unnamed statement, never prepared and reused, so
PostgreSQL plans each execution with its values, and each parameter's type
is left for the server to infer from its cast, so a value is read as that
type directly instead of converted from text. node-postgres and postgres.js
4 send it in one round trip; postgres.js 3 takes two when it has parameters,
as it describes a statement before binding it.

## Types

| PostgreSQL | TypeScript |
|------------|------------|
| `bool` | `boolean`; a parameter must be a boolean (`true` / `false`) — a string or a number is refused |
| `int2`, `int4`, `float4`, `float8`, `oid` | `number` |
| `int8` | `bigint`; parameters also take `number` and `string` |
| `numeric` | `string` (exact); parameters also take `number` |
| `text`, `varchar`, `char`, `uuid`, `inet`, `money`, … | `string` |
| `timestamptz` | `Temporal.Instant`; parameters also take `Temporal.ZonedDateTime` and `Date` (their instant). PostgreSQL stores no zone, only the instant: the offset it prints is the session's `TimeZone`, so keep a zone you need in a column of its own (`at.toZonedDateTimeISO(tz)`) |
| `timestamp` | `Temporal.PlainDateTime`; not a `Date`, whose wall-clock time depends on a zone (`date.toTemporalInstant().toZonedDateTimeISO(tz).toPlainDateTime()` picks one) |
| `date` | `Temporal.PlainDate`; not a `Date`, likewise |
| `time` | `Temporal.PlainTime` |
| `timetz` | `string`: Temporal has no time of day with an offset |
| `interval` | `Temporal.Duration`: years and months from its months, days, then hours down to microseconds from its time, as PostgreSQL prints it. A parameter's weeks are 7 days |
| `json`, `jsonb` | `JsonValue`; parameters take any value but `null` (stringified), and `null` where SQL NULL is allowed |
| `bytea` | `Buffer`; parameters take any `Uint8Array` |
| enum | a type the generated module exports, the union of its labels: `export type PostStatus = "draft" \| "published"` (see [Enums and domains](#enums-and-domains)) |
| domain | its base type, or the type `types` maps it to |
| composite, `ROW(...)` | an object with a property per field, each with its nullability — also inside an array (`array_agg(ROW(a, b))`, `ARRAY(SELECT ROW(…))`), a subquery or a `CASE` / `COALESCE` whose rows share one shape; a record whose fields nobody knows (a `RETURNS record` function's, rows of different shapes) reads as its text form |
| array | `T[]`, or `(T \| null)[]` unless the elements are known not null |
| `int2vector`, `oidvector` | `number[]` |
| range, multirange | `Range<T>`, `Range<T>[]` (`range(1, 10)`, `emptyRange` build them) |
| `vector`, `halfvec` (pgvector) | `number[]` |
| `hstore` | `Record<string, string \| null>` |
| `geometry`, `geography` (PostGIS) | `string`, its text form: hex EWKB (`0101000020E6…`), which parameters take too, as WKT or EWKT (`SRID=4326;POINT(1 2)`) do |
| anything else | `string`, its text form |

A value the query proves holds one of a few values reads as their literal
union — see [Values and ranges](#values-and-ranges).

Dates, times and durations are exact to PostgreSQL's microsecond: a
parameter's nanoseconds are sent too, and PostgreSQL rounds them. A Temporal
value is never `infinity`, so `to_char` and `extract` of a `date`,
`timestamp` or `timestamptz` parameter are never NULL; read from the
database, `infinity` and `-infinity` become the type's latest and earliest
values (`+275760-09-13` and `-271821-04-20`, a `Date`'s bounds).
PostgreSQL's ranges go further than Temporal's in places: a `timestamptz` or
`date` past year 275760, a `time` of `24:00:00`, an infinite `interval`, or
one whose fields disagree in sign (`1 mon -1 days`, which a Duration can't
hold) is a `DecodeError` — `justify_interval()` makes an interval's signs
agree.

In a `json`/`jsonb` parameter, `null` is SQL NULL, and the parameter's type
admits it only where the column or expression takes NULL. The JSON `null` is
`jsonNull` (`import { jsonNull } from "@cubos/typedpg"`); inside a value
(`{ a: null }`, `[1, null]`) a plain `null` is already the JSON one. Reading,
both come back as `null`.

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
Comments, whitespace, line endings (a CRLF checkout) and the case of unquoted
words do not count. An intentional edit that does not need to run again is
recorded with `acceptMigration` (or `typedpg migrate accept <name>`) without
running it.

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
| `roles` | The database roles every query is checked against. The generated types come from the first role's analysis — its search path decides the query's unqualified names — and every listed role must resolve the query the same way. Only roles the migrations create can be checked. In the multi-database form, each entry has its own. See [Role checks](#role-checks). |
| `types` | PG type → TypeScript type, as `"./src/types.ts#Prefs"` (relative to the config) or `"package#Type"`. A JSONB domain, an enum, a composite or any other type; an unqualified name is in `public`, a built-in type is `pg_catalog.<name>`. See [Mapping types](#mapping-types). |
| `runtime` | The module the generated code imports the runtime from (default `@cubos/typedpg`). |
| `databases` | Several databases: `{ "main": { "out": …, "migrations": … }, "warehouse": { … } }`, each with the keys above. A query belongs to the database whose module its `sql` comes from. |
| `simplify` | The query simplifications, on by default: `false` turns them all off, or an object sets one rule to `false` — `notIn`, `countExists`, `distinct`, `unionAll`, `fkJoin`. Where the analysis proves an equivalent query returns the same rows (from validated `NOT NULL`s and keys only), the one PostgreSQL plans better is sent: `x NOT IN (SELECT y …)` as an anti-join `NOT EXISTS` when neither side can be NULL, `count(…) > 0` as `EXISTS`, a `DISTINCT` that can't remove a row, a `UNION` whose branches can't share a row as `UNION ALL`, or a join a foreign key proves matches exactly one row none of whose columns are read. The generated module's doc comments show the SQL sent. For every database. |
| `lint` | `typedpg lint`'s settings: `failOn` (`"error"`, the default, `"warning"` or `"info"`), each rule's level — `nullComparison`, `unboundedWrite`, `notInNullable`, `createIndexBlocking`, `constraintValidating`, `setNotNullScan`, `tableRewrite`, `fkWithoutIndex`, `redundantIndex`, `redundantNullCheck`, `limitWithoutOrder`, `redundantLimit`, `redundantDistinct`, `constantCondition`, `unreachableCaseBranch`, `caseWithoutElse`, `redundantCast`, `redundantAlias`, `outerReferenceInSubquery`, `cartesianJoin`, `insertWithoutColumns`, `selfAssignment`, `unusedCte`, `simplifiableCase`, `likeUnescapedParam`, `redundantJoin`, `scalarSubqueryMayReturnMany`, `timestampTimezoneMix`, `integerDivision`, `leftJoinNullified`, `checkViolation`, `unstableOrder`, `redundantOrderBy`, `redundantGroupBy`, `countNotNull`, `nonSargablePredicate`, `floatEquality`, `fkTypeMismatch`, `missingLockTimeout`, `dropIndexBlocking`, `blockingRebuild`, `detachPartitionBlocking`, `validateInSameTransaction`, `attachPartitionScan`, `missingPrimaryKey`, `rlsPolicyNotEnforced`, `rlsNoPolicy`, `securityDefinerSearchPath`, `preferTimestamptz`, `preferIdentity`, `preferText`, `preferBigintPk`, `preferJsonb`, `fetchMayReturnMany`, `fetchAllSingleRow`, `unusedReturning`, `breaksDeployedQueries`, `jsonStringifyParam` — as `"error"`, `"warning"`, `"info"` or `"off"`, and `ignore`, glob patterns relative to the config's directory whose query findings are dropped and whose matching migration files skip the statement rules but keep their schema findings. For every database. |

Unknown keys are errors.

### Role checks

`roles` lists the roles the application connects as. Every query is analyzed as each of them, and one that may not run it is an error at the query — PostgreSQL's message, with the grant that would allow it. The generated types come from the first role's analysis: its search path decides what the query's unqualified names mean. Roles that resolve the same query to different types are an error naming both, since one module has one type per query:

```json
{ "out": "src/db.ts", "migrations": "migrations", "roles": ["app_role"] }
```

```sql
-- migrations/0002_app_role.sql
-- Numbered after the migration that creates `users`: the runner applies
-- migrations by numeric prefix.
CREATE ROLE app_role;
GRANT SELECT ON users TO app_role;
```

```text
src/orders.ts:14:5: error: permission denied for table orders
  help: grant it in a migration: GRANT SELECT ON public.orders TO app_role;
  note: role app_role has no SELECT on public.orders
```

The checks are PostgreSQL's own: USAGE on the schemas a query names, SELECT/INSERT/UPDATE/DELETE on its relations (per column where that is what the role is granted), EXECUTE on the routines it calls, and the sequence privileges of serial columns and `nextval` (an identity column's sequence needs no grant). Unqualified names resolve along the role's search path; views are read as their owner unless `security_invoker`. The bodies of the triggers a statement fires and of the routines it calls are checked recursively: a body runs with the statement's role, or with its owner's under `SECURITY DEFINER`, and what it reads and writes must be allowed too — except dynamic SQL (`EXECUTE`) and statements typedpg can't analyze. Only roles the migrations create can be checked — naming another is `role "x" does not exist (typedpg can check only the roles the migrations create)`. Row-level security and `COPY` are not checked. In the multi-database form, `roles` goes in each `databases` entry.

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

## Upgrading from 0.7

Dates, times and intervals are Temporal's types instead of `Date`, strings
and `Interval` (see [Types](#types)), which needs Node.js 26 and TypeScript
6.0:

- a `timestamptz` is a `Temporal.Instant` (`new Date(instant.epochMilliseconds)`
  for a `Date`; a parameter still takes one), a `timestamp` a
  `Temporal.PlainDateTime` — no longer a `Date` read and written as UTC;
- a `date` is a `Temporal.PlainDate` and a `time` a `Temporal.PlainTime`,
  no longer strings;
- an `interval` is a `Temporal.Duration`, no longer `{ months, days,
  microseconds }` or a string; the `Interval` type is gone;
- `MigrationStatus.appliedAt` is a `Temporal.Instant`.

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
