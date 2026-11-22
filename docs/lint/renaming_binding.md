# `renaming_binding`

A `sql!` binding `p = v` whose only effect is to rename the variable `v` to
the parameter `$p`.

| | |
|---|---|
| Default level | info |
| Applies to | `sql!` bindings (Rust only: `cargo typedpg lint`) |
| Fix | yes (`--fix`): rewrites every `$p` in the SQL to `$v` and deletes the binding |

## Why

`sql!` parameters capture the variable of their name. When a binding's value
is just another variable, the parameter could have used that variable's name
directly:

```rust
sql!(pool, "SELECT title FROM posts WHERE user_id = $user", user = user_id)
// is the same as
sql!(pool, "SELECT title FROM posts WHERE user_id = $user_id")
```

The second form has one name for one value: a reader of the SQL sees which
variable feeds the parameter without matching it against the binding list.
This is a style finding, so it defaults to `info`.

## What it reports

A binding `p = v` after the SQL literal of a `sql!` invocation, where `v` is
a plain identifier different from `p`, and renaming is safe and meaningful:

- `$p` (or `$..p`) occurs in the SQL;
- `v` contains a lowercase letter (a `SCREAMING_CASE` name is a constant by
  convention, and `$MAX_ROWS` would read as one);
- no parameter named `v` already exists in the SQL (the rename would merge
  two parameters);
- no other binding is named `v`, and no other binding also takes its value
  from `v`.

The help line uses the parameter's sigil: `$v`, or `$..v` when `p` occurs
only as a spread.

## Examples

### Bad

```rust
use typedpg::sql;

pub async fn titles(pool: &typedpg::Pool, user_id: i64) -> typedpg::Result<()> {
    let titles = sql!(pool, "SELECT title FROM posts WHERE user_id = $user", user = user_id)
        .fetch_all()
        .await?;
    Ok(())
}
```

```
info[renaming_binding]: `user = user_id` only renames the variable `user_id`
   ╭─[src/posts.rs:4:78]
 4 │     let titles = sql!(pool, "SELECT title FROM posts WHERE user_id = $user", user = user_id)
   ·                                                                              ──────────────
   ╰────
  help: write `$user_id` in the SQL and remove the binding (`cargo typedpg lint --fix` does it)
```

### Good

```rust
// The parameter has the variable's name.
sql!(pool, "SELECT title FROM posts WHERE user_id = $user_id")

// The value is an expression, not a variable.
sql!(pool, "SELECT title FROM posts WHERE user_id = $user", user = user.id)

// A SCREAMING_CASE constant: renaming would put $MAX_ROWS in the SQL.
sql!(pool, "SELECT title FROM posts ORDER BY id LIMIT $limit", limit = MAX_ROWS)

// Two parameters take the same variable: renaming both would merge them.
sql!(pool, "SELECT title FROM posts WHERE user_id = $user AND editor_id = $editor",
     user = user_id, editor = user_id)
```

## Fix

`--fix` rewrites every occurrence of the parameter in the SQL literal
(`$user` to `$user_id`, keeping a `?` / `!` nullability suffix and the `$..`
spread sigil as written) and deletes the binding:

```rust
let titles = sql!(pool, "SELECT title FROM posts WHERE user_id = $user_id")
```

Raw string literals (`r#"…"#`) are rewritten the same way.

## When it stays silent

- Any of the conditions above fails.
- The value is not a plain identifier (`&v`, `v.field`, `v.clone()`, a
  literal, a path).
- The parameter name is bound more than once (the macro rejects that).
- The binding is `v = v`: that is
  [`redundant_binding`](redundant_binding.md).

## Configuration

This rule exists only in `cargo typedpg lint`, and only the Rust
configuration accepts its key:

```toml
[package.metadata.typedpg.lint]
renaming_binding = "off"   # "error", "warning", "info" or "off"
fail_on = "warning"        # the severity that fails the run (default "error")
ignore = ["tests/**"]      # files whose findings are not reported
```

The TypeScript CLI has no bindings: `typedpg.config.json` rejects a
`renamingBinding` key as an unknown rule.

To allow it for one invocation, put an allow comment in the SQL literal:

```rust
sql!(pool, "-- typedpg-lint: allow(renaming_binding)\nSELECT title FROM posts WHERE user_id = $user", user = user_id)
```

## Related rules

- [`redundant_binding`](redundant_binding.md): a binding `x = x` that
  changes nothing.
