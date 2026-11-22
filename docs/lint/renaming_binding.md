# `renaming_binding`

A `sql!` binding `p = v` whose only effect is to rename the variable `v` to
the parameter `$p`.

| Level | Applies to | Fix |
|---|---|---|
| info | `sql!` bindings (Rust only) | writes `$v` in the SQL and removes the binding |

## Example

```rust
pub async fn titles(pool: &typedpg::Pool, user_id: i64) -> typedpg::Result<()> {
    let titles = sql!(pool, "SELECT title FROM posts WHERE user_id = $user", user = user_id)
        .fetch_all()
        .await?;
    Ok(())
}
```

```
info[renaming_binding]: `user = user_id` only renames the variable `user_id`
    ╭─[src/posts.rs:13:78]
 13 │     let titles = sql!(pool, "SELECT title FROM posts WHERE user_id = $user", user = user_id)
    ·                                                                              ──────────────
    ╰────
   help: write `$user_id` in the SQL and remove the binding (`cargo typedpg lint --fix` does it)
```

`--fix` gives `sql!(pool, "SELECT title FROM posts WHERE user_id = $user_id")`.
It rewrites every occurrence and keeps a `?`/`!` suffix and the `$..` of a
spread: `$..post_ids` with `post_ids = ids` becomes `$..ids`.

## Why

With the variable's name in the SQL, a reader sees what feeds the parameter
without matching it against the binding list.

## Reported when

The value is a plain identifier other than the parameter's name, and the
rename is safe: no parameter named `v` exists already, and no other binding
is named `v` or also takes `v`.

## Not reported

- A value that is not a plain identifier: `user = user.id`, `&v`,
  `v.clone()`, a literal, a path.
- A `SCREAMING_CASE` value, which would put `$MAX_ROWS` in the SQL:
  `limit = MAX_ROWS`.
- Two parameters bound to the same variable, which the rename would merge:
  `user = user_id, editor = user_id`.
- A parameter whose new name is already in the SQL:
  `"… user_id = $user_id AND editor_id = $editor", editor = user_id`.
- `v = v`: that is [`redundant_binding`](redundant_binding.md).

## Configuration

```toml
# Cargo.toml
[package.metadata.typedpg.lint]
renaming_binding = "warning"   # error | warning | info | off
```

`cargo typedpg lint` only: `typedpg.config.json` rejects `renamingBinding`
as an unknown rule.

Allow one invocation with `-- typedpg-lint: allow(renaming_binding)` in its
SQL literal. See [Configuring rules](README.md#configuring-rules).

## Related

- [`redundant_binding`](redundant_binding.md): a binding `x = x` that
  changes nothing.
