# typedpg

Compile-time verified PostgreSQL queries for Rust. **PostgreSQL only** -- no abstraction layer, no lowest-common-denominator SQL.

Write plain SQL, get full type safety. The `sql!` macro statically analyzes every query against your migration files during `cargo build` -- column types, parameter types, nullability, and type coercion errors are all caught before your code ships.

## Features

- **Compile-time checked** -- every query is validated against your actual schema. Typos in column names, wrong parameter types, invalid SQL, and type mismatches are all compiler errors. The analyzer reads your migrations and builds the schema in-memory -- no external process needed.
- **Real SQL** -- any syntax PostgreSQL accepts, `typedpg` accepts. CTEs, window functions, lateral joins, `DISTINCT ON`, `RETURNING`, `FOR UPDATE` -- if Postgres can parse and execute it, the macro will verify it. No restricted SQL subset, no Rust DSL.
- **Nullability-aware** -- the analyzer tracks nullability through JOINs, COALESCE, CASE, subqueries, and aggregates. `NOT NULL` columns become `T`, nullable columns become `Option<T>`.
- **Static type analysis** -- parameter types are inferred following PostgreSQL's own type resolution rules (operator/function resolution, implicit/assignment casts, common-type resolution). Type mismatches produce clear errors at compile time.
- **Zero runtime overhead** -- the macro generates concrete Rust structs with named fields. No runtime reflection, no `Box<dyn Any>`, no string-based column access.
- **PostgreSQL-native** -- first-class support for JSONB domains (`CREATE DOMAIN ... AS JSONB` mapped to Rust structs), enums (`CREATE TYPE ... AS ENUM` mapped to Rust enums), arrays, composite types, and advisory locks.

## Quick start

Add to your `Cargo.toml`:

```toml
[dependencies]
typedpg = "0.4"
deadpool-postgres = "0.14"
tokio-postgres = "0.7"
tokio = { version = "1", features = ["full"] }

[package.metadata.typedpg.database]
migrations = "./migrations"
```

Create `migrations/0001_create_users.sql`:

```sql
CREATE TABLE users (
    id    SERIAL PRIMARY KEY,
    name  TEXT NOT NULL,
    email TEXT NOT NULL UNIQUE,
    age   INT,
    bio   TEXT
);
```

Use it:

```rust
let users = sql!(pool, "SELECT id, name, age FROM users")
    .fetch_all()
    .await?;

for user in &users {
    // user.id  : i32       (NOT NULL column → plain type)
    // user.name: String    (NOT NULL column → plain type)
    // user.age : Option<i32>  (nullable column → Option)
    println!("{}: {}", user.id, user.name);
}
```

The macro reads your migration files, builds the schema in memory, and type-checks the query. The generated struct has correctly typed fields with proper nullability.

**Optional:** add a `build.rs` (with `typedpg` also under `[build-dependencies]`) so `cargo` re-runs `sql!` automatically whenever a migration file changes:

```rust
// build.rs
fn main() {
    typedpg::build::track_migrations();
}
```

Without it everything still works — you just have to invalidate the build manually after editing a migration.

## Compile-time error detection

Errors in your SQL are caught at build time, not at runtime:

```rust
// Column doesn't exist → compile error
sql!(pool, "SELECT nonexistent FROM users").fetch_all().await?;
//  error: column "nonexistent" does not exist

// Wrong type in WHERE → compile error
sql!(pool, "SELECT id FROM users WHERE name").fetch_all().await?;
//  error: type mismatch: text cannot be coerced to bool

// Wrong type in LIMIT → compile error
sql!(pool, "SELECT id FROM users LIMIT true").fetch_all().await?;
//  error: type mismatch: bool cannot be coerced to int8

// Parameter type mismatch → compile error (expects i32 for age column)
let flag: bool = true;
sql!(pool, "UPDATE users SET age = $flag").execute().await?;
//  error: type mismatch: bool cannot be coerced to int4
```

## The `sql!` macro

Four terminal methods for different use cases:

```rust
// fetch_all -- returns Vec<Row>
let users = sql!(pool, "SELECT id, name FROM users")
    .fetch_all().await?;

// fetch_one -- returns a single Row (errors if empty or >1)
let user = sql!(pool, "SELECT id, name FROM users WHERE id = $id", id = 1)
    .fetch_one().await?;

// fetch_optional -- returns Option<Row>
let maybe = sql!(pool, "SELECT id, name FROM users WHERE id = $id", id = 42)
    .fetch_optional().await?;

// execute -- returns u64 (affected rows)
let n = sql!(pool, "DELETE FROM users WHERE id = $id", id = 1)
    .execute().await?;
```

### `fetch_value` -- single-column shortcut

When your query returns a single column, `fetch_value` and `fetch_value_optional` extract the scalar directly -- no struct wrapping:

```rust
// Returns i64 directly, not a struct with a `count` field
let count = sql!(pool, "SELECT count(*) FROM users")
    .fetch_value().await?;
// count: i64

// Returns Option<String>
let name = sql!(pool, "SELECT name FROM users WHERE id = $id", id = 42)
    .fetch_value_optional().await?;
// name: Option<String>

// Also works with aggregates — nullable when no GROUP BY
let max_age = sql!(pool, "SELECT max(age) FROM users")
    .fetch_value().await?;
// max_age: Option<i32>
```

`fetch_value` is only generated when the query returns exactly one column. Multi-column queries use `fetch_one`/`fetch_all` with struct access.

### `fetch_stream` -- rows as they arrive

For large results, `fetch_stream` yields the typed rows as the server sends them instead of collecting a `Vec` first (`fetch_stream_as::<T>()` maps them to your own `FromRow` type):

```rust
use typedpg::stream::TryStreamExt;

let mut users = sql!(pool, "SELECT id, name FROM users")
    .fetch_stream().await?;
while let Some(user) = users.try_next().await? {
    println!("{} {}", user.id, user.name);
}
```

The stream is a nameable `typedpg::QueryStream<T>` (`Send` and `Unpin`). Run on a pool, it holds its connection until it is dropped; an error raised while the server produces rows (say, a division by zero in row 3) comes through the stream.

## Named parameters

Parameters use `$name` syntax. Values can be explicitly assigned or captured from scope:

```rust
// Explicit assignment
sql!(pool, "SELECT id FROM users WHERE email = $email", email = "alice@example.com")
    .fetch_one().await?;

// Scope capture — if a variable named `email` exists, just use $email
let email = "alice@example.com";
sql!(pool, "SELECT id FROM users WHERE email = $email")
    .fetch_one().await?;
```

Parameter types are inferred from context, following PostgreSQL's rules:

```rust
// $name → String (inferred from users.name column type)
// $min_age → i32 (inferred from users.age column type)
// $limit → i64 (LIMIT requires bigint)
sql!(pool,
    "SELECT id, name FROM users WHERE name = $name AND age > $min_age LIMIT $limit",
    name = "Alice", min_age = 18, limit = 10)
    .fetch_all().await?;
```

## Nullability

The analyzer tracks nullability precisely through the entire query:

```rust
// NOT NULL columns → plain types
let user = sql!(pool, "SELECT id, name FROM users WHERE id = $id", id = 1)
    .fetch_one().await?;
// user.id   : i32
// user.name : String

// Nullable columns → Option
let user = sql!(pool, "SELECT id, age, bio FROM users WHERE id = $id", id = 1)
    .fetch_one().await?;
// user.id  : i32
// user.age : Option<i32>   (age is nullable)
// user.bio : Option<String> (bio is nullable)

// LEFT JOIN makes the right side nullable
let row = sql!(pool,
    "SELECT u.name, p.title
     FROM users u LEFT JOIN posts p ON p.user_id = u.id
     WHERE u.id = $id", id = 1)
    .fetch_one().await?;
// row.name  : String         (left side, NOT NULL)
// row.title : Option<String> (right side of LEFT JOIN → nullable)

// COALESCE removes nullability
let row = sql!(pool, "SELECT COALESCE(age, 0) AS age FROM users WHERE id = $id", id = 1)
    .fetch_one().await?;
// row.age : i32  (COALESCE with non-null fallback → NOT NULL)

// COUNT is never null, even without GROUP BY
let count = sql!(pool, "SELECT count(*) FROM users")
    .fetch_value().await?;
// count: i64

// But SUM/AVG/MAX without GROUP BY are nullable (empty table → NULL)
let total = sql!(pool, "SELECT sum(age) FROM users")
    .fetch_value().await?;
// total: Option<i64>
```

### Functions and aggregates

What a builtin does with NULL counts, not just whether it is strict: `format`,
`array_remove`, `string_to_array` and `||` on arrays are NULL only for the
arguments that make them so; `jsonb_each` always fills `key` and `value`;
`EXTRACT(epoch FROM interval)` stays non-NULL for an infinite interval. An
aggregate keeping NULL inputs (`array_agg`, `json_agg`, `JSON_ARRAYAGG`, …) is
NULL only over no rows, `rank(…) WITHIN GROUP` and `regr_count` never are, and
an aggregate has rows in a group, in a window frame holding the current row,
over a constant source (`VALUES`, `generate_series(1, 10)`) and in a group
HAVING keeps (`HAVING max(x) > 0`; `HAVING count(x) > 0` also makes `max(x)`
non-NULL).

```rust
// array_agg keeps NULLs: never NULL in a group, its elements may be
let rows = sql!(pool,
    "SELECT u.id, array_agg(p.title) AS titles
     FROM users u LEFT JOIN posts p ON p.user_id = u.id GROUP BY u.id")
    .fetch_all().await?;
// rows[0].titles : Vec<Option<String>>

// HAVING proves the row's group has a non-NULL age
let total = sql!(pool, "SELECT sum(age) FROM users HAVING count(age) > 0")
    .fetch_value_optional().await?;
// total: Option<i64>  (no row, or the sum — never a NULL sum)
```

### Narrowing by conditions

A condition that must hold for a value to be read narrows it, the way
PostgreSQL's planner reasons about strict operators (`find_nonnullable_vars`,
`reduce_outer_joins`): `WHERE`, `HAVING`, an inner join's `ON`, an
aggregate's `FILTER` and a `CASE` branch's `WHEN`.

```rust
// A strict WHERE condition proves its column non-NULL
let rows = sql!(pool, "SELECT age FROM users WHERE age > $min", min = 18)
    .fetch_all().await?;
// rows[0].age : i32

// ...and turns a LEFT JOIN that filters on its nullable side into an inner one
let rows = sql!(pool,
    "SELECT u.name, p.title FROM users u LEFT JOIN posts p ON p.user_id = u.id
     WHERE p.published_at IS NOT NULL")
    .fetch_all().await?;
// rows[0].title : String

// The same expression, filtered then selected
let rows = sql!(pool,
    "SELECT data ->> 'email' AS email FROM users WHERE data ->> 'email' IS NOT NULL")
    .fetch_all().await?;
// rows[0].email : String

// A CASE branch knows what its WHEN ruled out
let rows = sql!(pool, "SELECT CASE WHEN age IS NULL THEN 0 ELSE age END AS age FROM users")
    .fetch_all().await?;
// rows[0].age : i32

// An aggregate reads only the rows its FILTER passes
let rows = sql!(pool,
    "SELECT u.id, array_agg(p.title) FILTER (WHERE p.id IS NOT NULL) AS titles
     FROM users u LEFT JOIN posts p ON p.user_id = u.id GROUP BY u.id")
    .fetch_all().await?;
// rows[0].titles : Option<Vec<String>>  (no NULL element; NULL with no post)
```

The schema counts too, taking its constraints to hold as declared (`NOT VALID`
included; `NOT ENFORCED` ones say nothing):

```rust
// A LEFT JOIN along a NOT NULL foreign key always finds its row
let rows = sql!(pool,
    "SELECT p.title, u.name FROM posts p LEFT JOIN users u ON u.id = p.user_id")
    .fetch_all().await?;
// rows[0].name : String

// CHECK constraints combine with what the query knows
// CHECK ((kind = 'card' AND card_last4 IS NOT NULL) OR (kind = 'iban' AND iban IS NOT NULL))
let rows = sql!(pool, "SELECT card_last4, coalesce(card_last4, iban) AS ref FROM payments WHERE kind = 'card'")
    .fetch_all().await?;
// rows[0].card_last4 : String, rows[0].ref : String

// ...written as an iff, with NOT, IS DISTINCT FROM or CASE, and refuted by
// `<>`, IN lists, `= ANY (…)` and orderings (`lvl >= 10` against `lvl < 10`)
// CHECK ((status = 'done') = (done_at IS NOT NULL))
let rows = sql!(pool, "SELECT done_at FROM tasks WHERE status IN ('done')")
    .fetch_all().await?;
// rows[0].done_at : OffsetDateTime

// A CASE without ELSE is NOT NULL when its WHENs cover every value: an
// enum's labels, a CHECK (kind IN (…)) list, both booleans, IS NULL and
// IS NOT NULL, `a > 0` and `a <= 0`
let rows = sql!(pool, "SELECT CASE status WHEN 'open' THEN 1 WHEN 'closed' THEN 2 END AS n FROM tickets")
    .fetch_all().await?;
// rows[0].n : i32  (status is a NOT NULL enum ('open', 'closed'))
```

A partition's bound counts as a constraint too (a range partition key, or a
list one with no NULL, is never NULL — in the partitioned table as well, when
no partition takes a NULL key), and so does a `MATCH FULL` foreign key: one of
its columns non-NULL makes all of them so.

A referenced table under row-level security doesn't count: its policies may
hide the row. The foreign key holds just the same through subqueries, CTEs,
views and join trees that pass the key — or every row of the referenced table —
through unchanged, and for a scalar subquery that looks the referenced row up:

```rust
let rows = sql!(pool,
    "SELECT p.title, (SELECT u.name FROM users u WHERE u.id = p.user_id) AS author
     FROM posts p")
    .fetch_all().await?;
// rows[0].author : String
```

Some queries always yield a row: an aggregate without `GROUP BY` (or
`HAVING`), a query without `FROM`, `VALUES`, such a lookup. A scalar subquery
like that is NULL only when its value is, and an outer join `ON true` to one
never null-extends it:

```rust
let rows = sql!(pool,
    "SELECT u.name, s.posts FROM users u
     LEFT JOIN LATERAL (SELECT count(*) AS posts FROM posts p WHERE p.user_id = u.id) s ON true")
    .fetch_all().await?;
// rows[0].posts : i64
```

A `FULL JOIN` row has one side or the other, so `coalesce(x.id, y.id)` over a
NOT NULL column of each is NOT NULL; a strict condition on a column a subquery
or view passes through narrows the row it came from; and a row of a strict
set-returning function in `FROM` (`generate_series(1, t.n)`,
`jsonb_array_elements(t.doc)`) proves its arguments were non-NULL.

Besides strict conditions, any condition that can't hold with a column NULL
counts: `coalesce(age, 0) > 0`, `age IS NOT DISTINCT FROM 5`, `(tenant_id, id)
= ($1, $2)`, `b IS NULL OR b > 0` (never NULL itself), or an `EXISTS` whose
subquery's `WHERE` is strict in an outer column. A condition on an expression
narrows the same expression where it is read again — `WHERE data ->> 'k' IS
NOT NULL` then `data ->> 'k'`, `HAVING max(x) > 0` then `max(x)` — unless it
runs a volatile function. Conditions that may hold for a NULL prove nothing:
`coalesce(age, 1) > 0`, `age IS DISTINCT FROM 5`, or an `OR` whose arms test
different columns.

A generated column (STORED or VIRTUAL) is its expression over the row, so
`GENERATED ALWAYS AS (coalesce(note, ''))` is never NULL. A view that is
automatically updatable reads its base table's CHECK constraints.

`RETURNING` knows what the statement wrote. An `INSERT` returns its values
and the column defaults; an `UPDATE` returns its `SET` values and whatever
its `WHERE` proved about the columns it keeps. `ON CONFLICT DO UPDATE`
returns the inserted row or the updated one, and `MERGE` one row per action
it runs, past its `WHEN` condition. Every returned row satisfies the table's
CHECK constraints. An automatically updatable view writes its base table's
rows, which need not pass its `WHERE`. A BEFORE ROW trigger or a rule may
rewrite the row, so the values aren't trusted there (only the constraints
are), and a `DO INSTEAD` rule or an `INSTEAD OF` trigger returns rows of its
own, of which nothing is known:

```rust
// DEFAULT 'draft', and a SET of a non-NULL value
let row = sql!(pool, "INSERT INTO posts (title) VALUES ($t) RETURNING status", t = "Hi")
    .fetch_one().await?;
// row.status : String
let rows = sql!(pool, "UPDATE users SET age = 18 WHERE age IS NULL RETURNING age")
    .fetch_all().await?;
// rows[0].age : i32
```

A data-modifying CTE that inserts one `VALUES` row (without a set-returning
function, which makes it any number of rows, nor `ON CONFLICT DO NOTHING`, a
`DO UPDATE ... WHERE` or a BEFORE ROW trigger, which may skip it) returns
exactly one row, so `(SELECT id FROM ins)` is that row's `id`.

### Nullability annotations

Override the inferred nullability when you know better than the analyzer. Use `!` to force non-nullable and `?` to force nullable.

**Columns** -- append `!` or `?` to the alias:

```rust
// p.title comes from a LEFT JOIN, so it's inferred as Option<String>.
// But if you know the join always matches, force it with "!":
let row = sql!(pool,
    r#"SELECT u.name, p.title as "title!"
       FROM users u LEFT JOIN posts p ON p.user_id = u.id
       WHERE u.id = $id"#, id = 1)
    .fetch_one().await?;
// row.title : String  (forced NOT NULL)

// name is NOT NULL, but you can force it nullable with "?":
let row = sql!(pool, r#"SELECT name as "name?" FROM users WHERE id = $id"#, id = 1)
    .fetch_one().await?;
// row.name : Option<String>  (forced nullable)
```

**Parameters** -- append `!` or `?` to the parameter name:

```rust
// Inferred from target column:
// age is nullable → $age accepts Option<i32>
sql!(pool, "UPDATE users SET age = $age WHERE id = $id", age = Some(25), id = 1)
    .execute().await?;

// name is NOT NULL → $name requires String (not Option)
sql!(pool, "UPDATE users SET name = $name WHERE id = $id", name = "Alice", id = 1)
    .execute().await?;

// Override:
// name is NOT NULL, but $name? forces it to accept Option<String>
sql!(pool, "UPDATE users SET name = $name? WHERE id = $id",
    name = Some("Alice"), id = 1)
    .execute().await?;

// age is nullable, but $age! forces it to require i32 (not Option)
sql!(pool, "UPDATE users SET age = $age! WHERE id = $id",
    age = 25, id = 1)
    .execute().await?;
```

## Bulk insert with `$..spread`

Insert multiple rows in a single statement:

```rust
struct NewUser { name: String, email: String }

let new_users = vec![
    NewUser { name: "Alice".into(), email: "alice@example.com".into() },
    NewUser { name: "Bob".into(),   email: "bob@example.com".into() },
];

sql!(pool, "INSERT INTO users (name, email) VALUES $..new_users { name, email }")
    .execute().await?;
```

The macro expands `$..new_users { name, email }` into a multi-row `VALUES` clause with proper parameter numbering. With no item the query still runs, as SQL would with no row: an empty spread next to other rows is left out, and a `VALUES` of nothing but empty spreads — which PostgreSQL has no syntax for — is written as a `SELECT` of no row (`SELECT * FROM (VALUES (NULL::type, …)) AS __typedpg_empty WHERE false`). So inserts in other CTEs of the statement happen, an aggregate over it has its row, and statement-level triggers fire.

## Lists with `IN $..list`

A spread without fields, right after `IN`, expands a list of values, each typed by the `IN`'s left side:

```rust
let ids: Vec<i64> = vec![1, 2, 3];

sql!(pool, "SELECT name FROM users WHERE id IN $..ids")   // WHERE id IN ($1, $2, $3)
    .fetch_all().await?;
```

An empty list is written `(SELECT NULL::<type> WHERE false)` — PostgreSQL has no syntax for one — and the query runs: `x IN` it is false, `x NOT IN` it true.

Above 1000 items, a spread is bound as arrays where that is provably the same query — the analyzer analyzes the array form and offers it only if the columns and parameters come out identical: `x IN $..ids` is written `(x = ANY($1::type[]))` (`NOT IN`: `<> ALL`), and a `VALUES` made of one rows spread `SELECT * FROM unnest($1::type1[], …) AS __typedpg_rows (column1, …)`. Large lists and batches then take a parameter per field instead of one per value — faster, and past PostgreSQL's 65535-parameter limit. Not offered where the form would differ: a field that is an array (unnest would flatten it) or a composite, a `VALUES` with written rows, a column whose nullability the array would change, an enum or JSON-domain field whose values may be `None`.

`id = ANY($ids)`, with an array parameter, is the same filter as a single placeholder, whatever the list's length. They differ when the statement runs with a generic plan (a prepared statement executed repeatedly): there PostgreSQL prunes the partitions of a partitioned table only for an `IN` list, and estimates its rows from the list's length instead of assuming 10 elements. An `IN` list is one statement text per length, and at most 65535 parameters.

## Bulk loading with `copy_in!`

For large loads, `copy_in!` streams rows through PostgreSQL's binary `COPY ... FROM STDIN` — much faster than `INSERT`, and with no limit on the number of rows:

```rust
use typedpg::copy_in;

let copied: u64 = copy_in!(pool, "users (name, email)", new_users { name, email })
    .await?;
```

- The target is a table and an optional column list (without one: every column but the generated ones), checked at compile time against your migrations — an unknown table or column, a generated column or a view without an `INSTEAD OF INSERT` trigger is a compile error with PostgreSQL's message.
- The rows are any `IntoIterator` (a `Vec`, `&slice`, or a lazy iterator); each item supplies the listed fields in the columns' order, typed by the columns — `Option<T>` only for nullable ones. Items are converted one at a time as the COPY consumes them.
- Enums, JSONB domains and arrays of them go through your `[package.metadata.typedpg.types]` mappings, as in `sql!`.
- If any row fails, the whole COPY is aborted and no row is kept. Inside a transaction, it commits or rolls back with it.

Use `$..spread` when you need `RETURNING` or `ON CONFLICT`; `copy_in!` when you just need the rows in.

## Enum types

PostgreSQL enums (`CREATE TYPE ... AS ENUM`) are supported out of the box. Without configuration, they map to `String`:

```sql
CREATE TYPE user_role AS ENUM ('admin', 'editor', 'viewer');

CREATE TABLE users (
    id   SERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    role user_role NOT NULL DEFAULT 'viewer'
);
```

```rust
// role is typed as String
let user = sql!(pool, "SELECT id, name, role FROM users WHERE id = $id", id = 1)
    .fetch_one().await?;
println!("Role: {}", user.role); // "admin", "editor", or "viewer"

// Parameters for enum columns also accept String
sql!(pool, "UPDATE users SET role = $role WHERE id = $id", role = "editor", id = 1)
    .execute().await?;
```

To get type-safe enum values instead of raw strings, map them in `Cargo.toml`:

```toml
[package.metadata.typedpg.types]
user_role = "crate::UserRole"
```

The macro will use your Rust type for serialization/deserialization. Your type must convert to/from `String`.

## Domain types (JSONB)

PostgreSQL domains (`CREATE DOMAIN`) are supported. A domain over `JSONB` can be mapped to a Rust struct that implements `serde::Serialize` and `serde::Deserialize`:

```sql
CREATE DOMAIN user_preferences AS JSONB;

CREATE TABLE profiles (
    user_id INT PRIMARY KEY REFERENCES users(id),
    preferences user_preferences
);
```

Without configuration, JSONB domains resolve to `serde_json::Value`:

```rust
// preferences: Option<serde_json::Value> (nullable JSONB domain)
let profile = sql!(pool, "SELECT user_id, preferences FROM profiles WHERE user_id = $id", id = 1)
    .fetch_one().await?;
```

With configuration, the macro automatically serializes/deserializes through your Rust type:

```toml
[package.metadata.typedpg.types]
user_preferences = "crate::domains::UserPreferences"
```

```rust
#[derive(Serialize, Deserialize)]
struct UserPreferences { theme: String, lang: String }

// preferences: Option<UserPreferences> -- automatic deserialization
let profile = sql!(pool, "SELECT user_id, preferences FROM profiles WHERE user_id = $id", id = 1)
    .fetch_one().await?;

if let Some(prefs) = &profile.preferences {
    println!("Theme: {}", prefs.theme);
}

// Parameters are also automatically serialized
let prefs = UserPreferences { theme: "dark".into(), lang: "pt-BR".into() };
sql!(pool, "UPDATE profiles SET preferences = $prefs WHERE user_id = $id", prefs = prefs, id = 1)
    .execute().await?;
```

Non-JSONB domains (e.g. `CREATE DOMAIN positive_int AS INT CHECK (VALUE > 0)`) are transparently unwrapped to their base type.

## PostgreSQL extensions

`CREATE EXTENSION` is processed by the DDL interpreter — the types, operators, and functions an extension installs become visible to the analyzer immediately. `citext`, `hstore`, `pg_trgm`, `uuid-ossp`, `btree_gin`, `vector` (pgvector), and the rest of the contrib bundle all work without extra setup.

For pgvector specifically, the macro auto-routes `vector` / `halfvec` / `sparsevec` columns to the [`pgvector`](https://docs.rs/pgvector) crate:

```sql
CREATE EXTENSION vector;

CREATE TABLE documents (
    id        SERIAL PRIMARY KEY,
    title     TEXT NOT NULL,
    embedding vector(384) NOT NULL
);
```

```rust
use pgvector::Vector;

// Top-5 nearest neighbours by cosine distance.
let query: Vector = embed("how do I write a CTE?");

let similar = sql!(pool,
    "SELECT id, title, embedding <=> $query AS distance
     FROM documents
     ORDER BY embedding <=> $query
     LIMIT $k",
    k = 5_i64)
    .fetch_all().await?;

for doc in &similar {
    // doc.id      : i32
    // doc.title   : String
    // doc.distance: f64               (`<=>` returns float8)
    // $query was inferred as pgvector::Vector from the operator's left operand
    println!("{}: {:.3}", doc.title, doc.distance);
}
```

The analyzer knows the `<=>`, `<->`, and `<#>` operators (and the matching index ops), so wrong operand types are caught at compile time just like for built-in types.

For extension types without a built-in mapping (e.g. `citext`, `hstore`), point them at the Rust type in `Cargo.toml`:

```toml
[package.metadata.typedpg.types]
"public.citext" = "String"
"public.hstore" = "::std::collections::HashMap<String, Option<String>>"
```

## Transactions

Pass a transaction directly to `sql!`:

```rust
let mut client = pool.get().await?;
let tx = client.transaction().await?;

sql!(&tx, "INSERT INTO users (name, email) VALUES ($name, $email)",
    name = "Charlie", email = "charlie@example.com")
    .execute().await?;

sql!(&tx, "UPDATE users SET name = $name WHERE email = $email",
    name = "Charles", email = "charlie@example.com")
    .execute().await?;

tx.commit().await?;
```

The `Executor` trait is implemented for:

| Type | Feature | Behavior |
|------|---------|----------|
| `deadpool_postgres::Pool` | `deadpool` (default) | Acquires a connection per query |
| `deadpool_postgres::Object` | `deadpool` (default) | Uses the pooled connection |
| `bb8::Pool<PostgresConnectionManager<Tls>>` | `bb8` | Acquires a connection per query |
| `tokio_postgres::Client` | always | Uses the raw client directly |
| `tokio_postgres::Transaction<'_>` | always | Executes within the transaction |

## Mapping rows to your own structs

By default `sql!` synthesises an anonymous result struct per query. When you want to return a struct you already own — say a domain type shared across several queries — derive `FromRow` and use the `fetch_*_as::<T>` methods:

```rust
#[derive(typedpg::FromRow)]
struct User {
    id: i32,
    name: String,
    email: Option<String>,
}

let user: User = sql!(pool, "SELECT id, name, email FROM users WHERE id = $id", id = 1)
    .fetch_one_as::<User>()
    .await?;

let users: Vec<User> = sql!(pool, "SELECT id, name, email FROM users")
    .fetch_all_as::<User>()
    .await?;

let maybe: Option<User> = sql!(pool, "SELECT id, name, email FROM users WHERE id = $id", id = 42)
    .fetch_optional_as::<User>()
    .await?;
```

Field names must match the query's output column names (a `"col!"` / `"col?"` alias names the column `col`). Each field's type must be its column's Rust type, or an `Option` of it, and this is checked at compile time: `age: i32` for a nullable `age`, or a field the query doesn't return, is a compile error pointing at the field. Rows are decoded the way `sql!` decodes its own struct, so mapped enum, JSONB-domain and composite fields work too.

## Multiple databases

When a single crate talks to more than one database, declare each one under `[package.metadata.typedpg.databases.<name>]` and pick which to use at the `sql!` site with a `db = <name>` prefix:

```toml
# Default database (used when `db = ...` is omitted)
[package.metadata.typedpg.database]
migrations = "./migrations"

# A second database, e.g. an analytics warehouse with its own schema
[package.metadata.typedpg.databases.analytics.database]
migrations = "./analytics/migrations"

[package.metadata.typedpg.databases.analytics.types]
"public.metric_kind" = "crate::analytics::MetricKind"
```

```rust
// Default database — schema comes from `./migrations`
let users = sql!(app_pool, "SELECT id, name FROM users")
    .fetch_all().await?;

// `db = analytics` switches the schema source to the named entry
let metrics = sql!(db = analytics, warehouse_pool, "SELECT kind, total FROM daily_metrics")
    .fetch_all().await?;
```

Each named entry is independent: its own migrations, its own `[migrations]` runner settings, its own `[types]` map. The runtime executor (`app_pool` / `warehouse_pool` above) is still passed by the caller — `typedpg` does not multiplex pools, it only points the compile-time analyzer at the right schema.

## Migrations

### CLI

Install the CLI and manage migrations from the terminal:

```bash
cargo install typedpg_cli
```

```bash
# Create a new migration (generates .sql and .down.sql files)
cargo typedpg migrate create add_posts_table

# Apply all pending migrations
cargo typedpg migrate up

# Show migration status
cargo typedpg migrate status

# Revert the last applied migration
cargo typedpg migrate down

# Revert a specific migration
cargo typedpg migrate down 20260406120000_add_posts_table

# Force revert even without a .down.sql file
cargo typedpg migrate down --force
```

The CLI reads configuration from `[package.metadata.typedpg]` in your `Cargo.toml` and connects using the `DATABASE_URL` environment variable (supports `.env` files).

### Programmatic

You can also run migrations programmatically at application startup:

```rust
use typedpg::migrate::{MigrationSource, MigrationsConfig, run};
use std::path::Path;

let source = MigrationSource::from_dir(Path::new("./migrations"))?;
let config = MigrationsConfig::default();
let applied = run(&mut client, &source, &config).await?;
```

For binaries that should ship without their migration files on disk, use the `embed_migrations!` macro to bake them into the binary at compile time:

```rust
use typedpg::migrate::{MigrationsConfig, run};

// Path is resolved relative to the invoking crate's CARGO_MANIFEST_DIR,
// like `include_str!`. The SQL contents land in the final binary as &'static str;
// editing a migration file re-triggers a rebuild automatically.
let source = typedpg::embed_migrations!("./migrations");
let config = MigrationsConfig::default();
let applied = run(&mut client, &source, &config).await?;
```

Migrations use advisory locks for safe concurrent deploys and wrap each migration in a transaction by default (opt-out with `-- no-transaction` as the first line of a migration file).

## Configuration

All configuration lives in your `Cargo.toml`:

```toml
[package.metadata.typedpg.database]
migrations = "./migrations"        # required — path to migration files
extra_migrations = ["../shared/migrations"]  # optional — extra migration dirs
                                             # included in the compile-time
                                             # schema only (NOT applied at
                                             # runtime); use when another crate
                                             # owns shared tables

[package.metadata.typedpg.migrations]
table = "public._migrations"       # optional — migration tracking table
lock_id = 713705                   # optional — advisory lock ID
use_transaction = true             # optional — wrap each migration in a tx
fail_on_drift = true               # optional — abort if an already-applied
                                   # migration file has been edited since

[package.metadata.typedpg.types]       # optional — PG type → Rust type
user_preferences = "crate::UserPrefs"  # JSONB domain → Rust struct (serde)
user_role = "crate::UserRole"          # PG enum → Rust enum (Display + FromStr)
```

A key typedpg does not recognize is a compile error rather than silently
ignored.

For projects with more than one database see the [Multiple databases](#multiple-databases) section above.

## How it works

The `sql!` macro performs **fully static analysis** at compile time:

1. Reads your migration files from the configured path
2. Parses the DDL statements with PostgreSQL 18's own parser ([libpg_query](https://github.com/pganalyze/libpg_query), bundled via `typedpg_pg_query`)
3. Builds an in-memory schema snapshot by applying each migration's DDL on top of a built-in PostgreSQL 18 catalog seed
4. Parses your SQL query and resolves column types, parameter types, and nullability against the snapshot
5. Generates a concrete Rust struct with correctly typed fields

Everything runs in-process during `cargo build`. No external dependencies, fast builds, fully reproducible.

Extensions are supported via built-in SQL definitions that the DDL interpreter processes automatically when it sees `CREATE EXTENSION`.

### Faster dev builds

Cargo compiles proc macros and their dependencies **unoptimized** in dev builds, so the analyzer behind `sql!` runs at debug speed — several times slower than it needs to. Opting it into optimization in your `Cargo.toml` makes each `sql!` and each migration replay much cheaper; the analyzer is then compiled optimized once and cached:

```toml
[profile.dev.package.typedpg_analyzer]
opt-level = 3

[profile.dev.package.typedpg_pg_query]
opt-level = 3
```

Measured on a schema of ~300 tables: replaying the migrations goes from ~0.5 s to ~0.2 s per crate build, and analyzing a query from ~0.3 ms to ~0.04 ms.

## Requirements

- Rust 1.89+

## License

Proprietary -- Cubos Tecnologia.
