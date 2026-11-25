# `like_unescaped_param`

A `LIKE` / `ILIKE` pattern built from a parameter whose `%` and `_` are not
escaped.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| correctness | error | queries | — |

## Example

```sql
CREATE TABLE users (id bigint PRIMARY KEY, name text NOT NULL);

SELECT id FROM users WHERE name ILIKE '%' || $q || '%';
SELECT id FROM users WHERE name LIKE concat($p::text, '%');
```

```
error[correctness/like_unescaped_param]: the pattern concatenates a parameter unescaped: a `%` or `_` in it matches any characters
   ╭─[queries.sql:2:39]
 2 │ SELECT id FROM users WHERE name ILIKE '%' || $q || '%';
   ·                                       ────────────────
   ╰────
  help: escape \, % and _ in the parameter (replace(replace(replace($q, '\', '\\'), '%', '\%'), '_', '\_')), or use starts_with / strpos
```

## Why

`%`, `_` and `\` keep their pattern meaning inside the parameter: a search
for `50%` matches everything containing `50`, and a search for `%` returns
the whole table. It is not SQL injection, but user input controls the
pattern, and can turn a cheap search into a slow one. It is a correctness
error: a search for `50%` silently matches every row containing `50`. A
project that wants users to type wildcards can set
`like_unescaped_param = "off"`.

## Reported when

The pattern of `LIKE`, `ILIKE`, `NOT LIKE`, `NOT ILIKE` (or `~~`, `~~*`,
`!~~`, `!~~*`) is a concatenation (`||` or `concat(…)`, nested any way)
containing both:

- a parameter, bare or cast to a string type (`$q`, `$q::text`);
- a string literal with `%` or `_`.

## Not reported

- The pattern is the parameter alone: `LIKE $pattern`.
- No literal with `%` or `_`: `$q || 'abc'`.
- The parameter goes through any function call, assumed to escape it:
  `replace(replace(replace($q, '\', '\\'), '%', '\%'), '_', '\_')`.
- A cast through a non-string type: `$n::bigint::text`, `$id::uuid::text`.
- A parameter of a type whose text has no wildcards: `smallint`, `integer`,
  `bigint`, `numeric`, `real`, `double precision`, `oid`, `boolean`, `uuid`,
  `date`, `timestamp`, or a domain over one. `timestamptz`, `timetz` and
  `interval` are still reported (a time zone abbreviation can contain `_`).
- `SIMILAR TO` and regular expressions. `starts_with(name, $q)` and
  `strpos(name, $q) > 0` need no escaping.

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
like_unescaped_param = "warning"   # error | warning | info | off
```
```json
// typedpg.config.json
{ "lint": { "likeUnescapedParam": "warning" } }
```

Allow one query with `-- typedpg-lint: allow(like_unescaped_param)` in its
text. See [Configuring rules](README.md#configuring-rules).
