# `redundant_binding`

A `sql!` binding `x = x` (or a bare `x`) for a parameter that would capture
the variable `x` without it.

| | |
|---|---|
| Default level | warning |
| Applies to | `sql!` bindings (Rust only: `cargo typedpg lint`) |
| Fix | yes (`--fix`): deletes the binding |

## Why

In the `sql!` macro, a `$name` parameter with no binding captures the Rust
variable `name` in scope, like `format!("{name}")` does. A binding is only
needed to give the parameter some other value:

```rust
sql!(pool, "SELECT title FROM posts WHERE id = $id")              // captures `id`
sql!(pool, "SELECT title FROM posts WHERE id = $id", id = post.id) // binds an expression
```

`id = id`, or a bare `id` after the SQL, binds the parameter to the very
variable it would capture anyway. It is noise that makes readers look for a
difference that is not there, and it drifts: rename the variable and the
binding has to change in two places.

## What it reports

A binding after the SQL literal of a `sql!` invocation that is either:

- `name = name`, where the value is a plain identifier equal to the binding's
  name; or
- a bare `name` (the macro's shorthand for `name = name`).

The message shows the parameter as the SQL writes it: `$name`, or `$..name`
when it occurs only as a spread (`VALUES $..rows`).

Nothing is reported for a name bound twice (the macro rejects that itself),
or when the SQL template does not lex (the analysis reports that error).

## Examples

### Bad

```rust
use typedpg::sql;

pub async fn post_title(pool: &typedpg::Pool, id: i64) -> typedpg::Result<()> {
    let title = sql!(pool, "SELECT title FROM posts WHERE id = $id", id = id)
        .fetch_optional()
        .await?;
    Ok(())
}
```

```
warning[redundant_binding]: `id = id` binds `$id` to the variable it captures without a binding
   ╭─[src/posts.rs:4:70]
 4 │     let title = sql!(pool, "SELECT title FROM posts WHERE id = $id", id = id)
   ·                                                                      ───────
   ╰────
  help: remove the binding: a parameter without one captures the variable of its name (`cargo typedpg lint --fix` removes it)
```

### Good

```rust
// The parameter captures `id`.
sql!(pool, "SELECT title FROM posts WHERE id = $id")

// A binding with a real expression.
sql!(pool, "SELECT title FROM posts WHERE user_id = $user", user = user.id)
```

A binding to a different variable (`user = user_id`) is not this rule's
finding; see [`renaming_binding`](renaming_binding.md).

## Fix

`--fix` deletes the binding, with its separating comma:

```rust
let title = sql!(pool, "SELECT title FROM posts WHERE id = $id")
```

The fix edits only the Rust source; the SQL is unchanged.

## When it stays silent

- The value is anything but a plain identifier: `&id`, `post.id`, `(id)`,
  `id.clone()`, a literal, a path such as `ids::FIRST`, or `self`.
- The same name is bound more than once.

## Configuration

This rule exists only in `cargo typedpg lint`, and only the Rust
configuration accepts its key:

```toml
[package.metadata.typedpg.lint]
redundant_binding = "error"   # "error", "warning", "info" or "off"
fail_on = "warning"           # the severity that fails the run (default "error")
ignore = ["tests/**"]         # files whose findings are not reported
```

The TypeScript CLI has no bindings: `typedpg.config.json` rejects a
`redundantBinding` key as an unknown rule.

To allow it for one invocation, put an allow comment in the SQL literal:

```rust
sql!(pool, "-- typedpg-lint: allow(redundant_binding)\nSELECT title FROM posts WHERE id = $id", id = id)
```

## Related rules

- [`renaming_binding`](renaming_binding.md): a binding that only renames a
  variable.
