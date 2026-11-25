# `timestamp_timezone_mix`

An implicit conversion between `timestamp` (or `date`) and `timestamptz`,
whose result depends on the session's `TimeZone`.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| suspicious | warning | queries | — |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, created_at timestamptz NOT NULL);
CREATE TABLE posts (id bigint PRIMARY KEY, published_at timestamp);

SELECT id FROM posts WHERE published_at > now() - interval '1 day';
UPDATE posts SET published_at = now() WHERE id = $id;
SELECT id FROM users WHERE created_at >= current_date;
```

```
warning[suspicious/timestamp_timezone_mix]: `>` compares timestamp without time zone with timestamp with time zone, converting the timestamp in the session's TimeZone setting
   ╭─[queries.sql:2:28]
 2 │ SELECT id FROM posts WHERE published_at > now() - interval '1 day';
   ·                            ────────────
   ╰────
  help: convert it in an explicit time zone: `value AT TIME ZONE 'UTC'`

warning[suspicious/timestamp_timezone_mix]: timestamp with time zone value implicitly converted to timestamp without time zone: the result depends on the session's TimeZone setting
   ╭─[queries.sql:4:33]
 4 │ UPDATE posts SET published_at = now() WHERE id = $id;
   ·                                 ─────
   ╰────
  help: convert it in an explicit time zone: `value AT TIME ZONE 'UTC'`

warning[suspicious/timestamp_timezone_mix]: `>=` compares date with timestamp with time zone, converting the date in the session's TimeZone setting
   ╭─[queries.sql:6:42]
 6 │ SELECT id FROM users WHERE created_at >= current_date;
   ·                                          ────────────
   ╰────
  help: convert it in an explicit time zone: `value::timestamp AT TIME ZONE 'UTC'`
```

## Why

A `timestamp` is a wall-clock reading; a `timestamptz` is an instant.
PostgreSQL converts between them in the session's `TimeZone`, which may come
from the server, the role, the driver or a stray `SET`. The same query on
the same data then matches different rows, or stores a different value, per
connection. A `date` becomes midnight in that zone.

## Reported when

A `timestamp` or `date` value meets a `timestamptz` without an explicit
conversion, in either direction:

- a comparison (`<`, `=`, `BETWEEN`, …), reported on the side without a time
  zone;
- arithmetic or a function argument;
- `CASE`, `COALESCE` or `UNION` branches resolving to the other type;
- a value stored in a column of the other type (`UPDATE … SET`,
  `INSERT … VALUES`, `INSERT … SELECT`).

Domains count as their base type.

## Not reported

- `AT TIME ZONE`:
  `WHERE published_at AT TIME ZONE 'UTC' > now() - interval '1 day'`.
- An explicit cast: `published_at::timestamptz > now()`. It still uses the
  session's zone, but says so.
- A parameter or an untyped literal, which take the column's type:
  `published_at > $since`, `published_at > '2026-01-01'`.
- `date` with `timestamp`: `published_at > current_date`.
- A `date` formatted as a date: `to_char(current_date, 'YYYY-MM-DD')`. The
  date becomes midnight in the session's zone — 01:00 of the same day when
  that midnight doesn't exist — and `to_char` prints it back in the same
  zone, so a format naming only date fields prints the date itself whatever
  the zone: `Y`, `YY`, `YYY`, `YYYY`, `IYYY`, `IYY`, `IY`, `I`, `MM`, `MON`,
  `MONTH`, `RM`, `Q`, `CC`, `J`, `DD`, `DDD`, `D`, `DAY`, `DY`, `ID`,
  `IDDD`, `WW`, `W`, `IW`, `BC`, `AD`, with `FM`/`FX` prefixes, a `TH`
  suffix, literal text and quoted strings. `to_char(published_at, 'YYYY')`
  on a `timestamp` column is not this case (`to_char(timestamp, text)`
  converts nothing). A format naming a time-of-day or zone field (`HH`,
  `HH12`, `HH24`, `MI`, `SS`, `MS`, `US`, `FF1`–`FF6`, `SSSS`, `AM`, `PM`,
  `A.M.`, `P.M.`, `TZ`, `tz`, `TZH`, `TZM`, `OF`), a format that isn't a
  constant string, and a user-defined `to_char(timestamptz, text)` earlier
  on the search path are reported.
- Conversions inside a column default or a called SQL function.

No fix is offered: only the application knows which zone the wall-clock
values are in. Often the real fix is a `timestamptz` column
([`prefer_timestamptz`](prefer_timestamptz.md)).

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
timestamp_timezone_mix = "error"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "timestampTimezoneMix": "error" } }
```

Allow one query with `-- typedpg-lint: allow(timestamp_timezone_mix)` in
its text. See [Configuring rules](README.md#configuring-rules).

## Related

- [`prefer_timestamptz`](prefer_timestamptz.md): a `timestamp without time
  zone` column.
