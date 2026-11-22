# `timestamp_timezone_mix`

An implicit conversion between `timestamp` (or `date`) and `timestamptz`,
whose result depends on the session's `TimeZone` setting.

| | |
|---|---|
| Default level | warning |
| Applies to | queries |
| Fix | no |

## Why

`timestamp without time zone` is a wall-clock reading with no zone attached;
`timestamp with time zone` (`timestamptz`) is an absolute instant. To compare
or combine the two, PostgreSQL has to decide which zone the wall-clock value
was in, and it silently uses the session's `TimeZone` setting. `date` is the
same: a `date` becomes midnight in the session's zone.

So the same query, on the same data, returns different rows depending on a
connection setting:

```sql
-- published_at is timestamp; now() is timestamptz.
SELECT id FROM posts WHERE published_at > now() - interval '1 day';
```

On a connection with `TimeZone = 'UTC'` this is "the last 24 hours". On one
with `TimeZone = 'America/Sao_Paulo'` the window is shifted by three hours.
The setting comes from the server's `postgresql.conf`, the role, the
database, the driver or a `SET` in some other piece of code, so the bug shows
up as "the report is off by a few hours on staging". Writes have the same
problem: `SET published_at = now()` stores the wall clock of whatever zone
the connection happens to be in.

## What it reports

typedpg records, for every value, the type it has and the type its context
converts it to, and for every operator the types it combines. The rule
reports:

- a value implicitly converted between `timestamptz` and `timestamp` or
  `date`, in either direction: an argument of arithmetic or of a function
  (`takes_tz(published_at)`, `age(at_tz, at)`), a branch of `CASE` /
  `COALESCE` or an arm of `UNION` whose types resolve to the other type, a
  value assigned to a column of the other type (`UPDATE … SET`, `INSERT …
  VALUES`, `INSERT … SELECT`);
- a comparison through a cross-type operator (`timestamp < timestamptz`,
  `date = timestamptz`, `BETWEEN`), which converts inside the operator
  rather than converting an operand. The finding is placed on the side
  without a time zone.

A domain over one of these types counts as its base type.

## Examples

```sql
CREATE TABLE users (
    id bigint PRIMARY KEY,
    email text NOT NULL UNIQUE,
    created_at timestamptz NOT NULL
);

CREATE TABLE posts (
    id bigint PRIMARY KEY,
    title text NOT NULL,
    published_at timestamp
);
```

### Bad

```sql
SELECT id FROM posts WHERE published_at > now() - interval '1 day';
```

```
warning[timestamp_timezone_mix]: `>` compares timestamp without time zone with timestamp with time zone, converting the timestamp in the session's TimeZone setting
   ╭─[queries.sql:1:28]
 1 │ SELECT id FROM posts WHERE published_at > now() - interval '1 day';
   ·                            ────────────
   ╰────
  help: convert it in an explicit time zone: `value AT TIME ZONE 'UTC'`
```

```sql
UPDATE posts SET published_at = now() WHERE id = $id;
```

```
warning[timestamp_timezone_mix]: timestamp with time zone value implicitly converted to timestamp without time zone: the result depends on the session's TimeZone setting
   ╭─[queries.sql:1:33]
 1 │ UPDATE posts SET published_at = now() WHERE id = $id;
   ·                                 ─────
   ╰────
  help: convert it in an explicit time zone: `value AT TIME ZONE 'UTC'`
```

`current_date` is a `date`; comparing it with a `timestamptz` turns it into
midnight in the session's zone:

```sql
SELECT id FROM users WHERE created_at >= current_date;
```

```
warning[timestamp_timezone_mix]: `>=` compares date with timestamp with time zone, converting the date in the session's TimeZone setting
   ╭─[queries.sql:1:42]
 1 │ SELECT id FROM users WHERE created_at >= current_date;
   ·                                          ────────────
   ╰────
  help: convert it in an explicit time zone: `value::timestamp AT TIME ZONE 'UTC'`
```

### Good

```sql
-- AT TIME ZONE says which zone the wall-clock value is in.
SELECT id FROM posts
WHERE published_at AT TIME ZONE 'UTC' > now() - interval '1 day';

UPDATE posts SET published_at = now() AT TIME ZONE 'UTC' WHERE id = $id;

-- A parameter takes the column's type: nothing is converted in SQL.
SELECT id FROM posts WHERE published_at > $since;

-- An untyped literal is read as the column's type.
SELECT id FROM posts WHERE published_at > '2026-01-01';

-- Both sides are timestamp without time zone.
SELECT id FROM posts WHERE published_at > localtimestamp - interval '1 day';
```

An explicit cast (`published_at::timestamptz > now()`) is not reported
either: it still uses the session's zone, but it says so in the code.
Conversions that happen inside a column default or inside a SQL function the
query calls are not the query's own and are not reported.

The cleanest fix is often in the schema: store instants as `timestamptz`
(see [`prefer_timestamptz`](prefer_timestamptz.md)).

## Fix

None. Which zone the wall-clock values were written in is something only
the application knows, so the rule cannot pick the `AT TIME ZONE` argument.

## When it stays silent

- The value is a parameter or an untyped literal: its type comes from its
  context, so nothing is converted.
- The conversion is explicit: a cast or `AT TIME ZONE`.
- `date` compared with `timestamp` (without time zone): that conversion does
  not involve a time zone.

## Configuration

```toml
[package.metadata.typedpg.lint]
timestamp_timezone_mix = "error"   # "error", "warning", "info" or "off"
```

```json
{
  "lint": { "timestampTimezoneMix": "error" }
}
```

`fail_on` / `failOn` sets the severity that fails the run (default `error`),
and `ignore` lists glob patterns of files whose query findings are dropped.
For one statement:

```sql
-- typedpg-lint: allow(timestamp_timezone_mix)
SELECT id FROM posts WHERE published_at > now() - interval '1 day';
```

## Related rules

- [`prefer_timestamptz`](prefer_timestamptz.md): a `timestamp without time
  zone` column in the schema.
