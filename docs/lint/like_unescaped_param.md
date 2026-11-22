# `like_unescaped_param`

A `LIKE` / `ILIKE` pattern built by concatenating a parameter with wildcards,
without escaping the parameter's own `%` and `_`.

| | |
|---|---|
| Default level | off (set a level to enable it) |
| Applies to | queries |
| Fix | no |

## Why

The usual "contains" search is written `name ILIKE '%' || $q || '%'`. In a
`LIKE` pattern, `%` matches any run of characters, `_` matches any single
character and `\` escapes the next character. Those characters keep their
meaning when they arrive inside the parameter. A user who searches for `50%`
gets every name containing `50`, a search for `_` matches every non-empty
name, and a search for `%` returns the whole table. It is not SQL injection
(the parameter is still a bound value), but user input is controlling the
pattern, which can also turn a cheap search into a slow one.

The rule is off by default: a project that does want users to type
wildcards, or that only passes trusted values, would find it noisy.

## What it reports

A `LIKE`, `ILIKE`, `NOT LIKE` or `NOT ILIKE` (or the operators `~~`, `~~*`,
`!~~`, `!~~*`) whose pattern, the right-hand side, is a concatenation (`||`
or `concat(…)`, nested any way) that contains both:

- a bare parameter, possibly cast to a string type (`$q`, `$q::text`), and
- a string literal with a `%` or `_` in it.

What counts as escaped or harmless is decided from the parse tree and the
parameter's inferred type:

- A parameter inside any other function call (`replace($q, …)`,
  `lower($q)`, your own escaping function) is assumed escaped.
- A cast chain through a non-string type (`$n::integer::text`,
  `$id::uuid::text`) parses the value, so no wildcard survives it.
- A parameter whose inferred type prints only digits and separators cannot
  carry wildcards: `smallint`, `integer`, `bigint`, `numeric`, `real`,
  `double precision`, `oid`, `boolean`, `uuid`, `date` and `timestamp`
  (or a domain over one). Types whose text depends on session settings
  (`timestamptz`, `timetz`, `interval`) are still reported: a time zone
  abbreviation can contain `_`.

The finding spans the pattern expression.

## Examples

```sql
CREATE TABLE users (
    id bigint PRIMARY KEY,
    email text NOT NULL UNIQUE,
    name text,
    created_at timestamptz NOT NULL
);

CREATE TABLE posts (
    id bigint PRIMARY KEY,
    title text NOT NULL
);
```

With the rule enabled (`like_unescaped_param = "warning"`):

### Bad

```sql
SELECT id FROM users WHERE name ILIKE '%' || $q || '%';
```

```
warning[like_unescaped_param]: the pattern concatenates a parameter unescaped: a `%` or `_` in it matches any characters
   ╭─[queries.sql:1:39]
 1 │ SELECT id FROM users WHERE name ILIKE '%' || $q || '%';
   ·                                       ────────────────
   ╰────
  help: escape \, % and _ in the parameter (replace(replace(replace($q, '\', '\\'), '%', '\%'), '_', '\_')), or use starts_with / strpos
```

### Good

```sql
-- Escape the backslash first, then % and _ (backslash is LIKE's default
-- escape character).
SELECT id FROM users
WHERE name ILIKE '%' || replace(replace(replace($q, '\', '\\'), '%', '\%'), '_', '\_') || '%';

-- No pattern at all.
SELECT id FROM users WHERE strpos(lower(name), lower($q)) > 0;

-- The caller passes the whole pattern: wildcards are the point.
SELECT id FROM users WHERE name LIKE $pattern;

-- The cast through bigint rejects anything but a number.
SELECT id FROM posts WHERE title LIKE '%' || $id::bigint::text || '%';
```

For a prefix search, `starts_with(name, $q)` needs no escaping at all.

## Fix

None. The right escaping depends on the pattern's `ESCAPE` character and on
whether the application escapes before binding, so the rule only points at
the pattern.

## When it stays silent

- The pattern is the parameter alone (`LIKE $pattern`), or the concatenation
  has no literal containing `%` or `_` (`$q || 'abc'`).
- The parameter is passed through any function call; the rule does not check
  that the function really escapes.
- The parameter's type cannot print `%`, `_` or `\` (see above).
- Only `LIKE`-family operators are read; `SIMILAR TO` and regular expression
  operators are not.

## Configuration

The rule is off until you give it a level:

```toml
[package.metadata.typedpg.lint]
like_unescaped_param = "warning"   # "error", "warning", "info" or "off"
```

```json
{
  "lint": { "likeUnescapedParam": "warning" }
}
```

`fail_on` / `failOn` sets the severity that fails the run (default `error`),
and `ignore` lists glob patterns of files whose query findings are dropped.
To accept one deliberate pattern:

```sql
-- typedpg-lint: allow(like_unescaped_param)
SELECT id FROM users WHERE name LIKE '%' || $q || '%';
```

## Related rules

- [`null_comparison`](null_comparison.md): another comparison that does not
  do what it reads like.
