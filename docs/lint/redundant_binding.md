# `redundant_binding`

A `sql!` binding `x = x`, or a bare `x`, for a parameter that captures the
variable `x` without it.

| Category | Level | Applies to | Fix |
|---|---|---|---|
| redundant | info | `sql!` bindings (Rust only) | removes the binding |

## Example

```rust
pub async fn post_title(pool: &typedpg::Pool, id: i64) -> typedpg::Result<()> {
    let title = sql!(pool, "SELECT title FROM posts WHERE id = $id", id = id)
        .fetch_optional()
        .await?;
    Ok(())
}
```

```
info[redundant/redundant_binding]: `id = id` binds `$id` to the variable it captures without a binding
   ╭─[src/posts.rs:6:70]
 6 │     let title = sql!(pool, "SELECT title FROM posts WHERE id = $id", id = id)
   ·                                                                      ───────
   ╰────
  help: remove the binding: a parameter without one captures the variable of its name (`cargo typedpg lint --fix` removes it)
```

`--fix` gives `sql!(pool, "SELECT title FROM posts WHERE id = $id")`. A
spread is shown as written: `` `ids = ids` binds `$..ids` … ``.

## Why

A `$name` parameter without a binding captures the variable `name`, like
`format!("{name}")`. Binding it to that same variable is noise that sends
readers looking for a difference, and renaming the variable means editing
two places.

## Not reported

- A value that is not a plain identifier: `id = post.id`, `id = &id`,
  `id.clone()`, a literal, a path, `self`.
- A name bound twice (the macro rejects it).
- A binding to another variable, `user = user_id`: that is
  [`renaming_binding`](renaming_binding.md).

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
redundant_binding = "error"   # error | warning | info | off
```

`cargo typedpg lint` only: `typedpg.config.json` rejects
`redundantBinding` as an unknown rule.

Allow one invocation with `-- typedpg-lint: allow(redundant_binding)` in
its SQL literal. See [Configuring rules](README.md#configuring-rules).

## Related

- [`renaming_binding`](renaming_binding.md): a binding that only renames a
  variable.
