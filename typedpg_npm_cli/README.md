# typedpg for TypeScript: the command and bundler plugins

This is the development package of
[`@cubos/typedpg`](https://www.npmjs.com/package/@cubos/typedpg), the
TypeScript runtime. Install it as a devDependency:

```sh
npm install --save-dev @cubos/typedpg-cli
npm install @cubos/typedpg
```

It gives you:

- the `typedpg` command — `gen`, `check`, `build`, `migrate` and `serve` —
  a native binary installed for your platform as an optional dependency
  (`@cubos/typedpg-cli-linux-x64-gnu`, `-darwin-arm64`, …), so a production
  install of `@cubos/typedpg` never downloads it;
- the bundler plugins of `.sql` files: `@cubos/typedpg-cli/esbuild`,
  `/vite`, `/rollup` and `/webpack`, which read the cache `typedpg build`
  fills (see [the runtime's README](../typedpg_npm/README.md#importing-sql-files)).

```sh
npx typedpg gen            # write the generated modules
npx typedpg build          # compile the .sql files into the loaders' cache
npx typedpg migrate up     # DATABASE_URL (a .env is read), or --url
```

The runtime's Node and Bun loaders (`@cubos/typedpg/register` and
`@cubos/typedpg/bun`) stay in `@cubos/typedpg`: they run in production from
the cache `typedpg build` filled, with no binary.

`TYPEDPG_BINARY` points the command at a binary of your own instead of the
platform package.

## In this repository

The CLI imports the runtime's cache and binary lookup through the runtime's
internal `@cubos/typedpg/internal` subpath — not a public API. Its
`package.json` depends on the runtime by path (`file:../typedpg_npm`) so the
two build together; `typedpg_npm/scripts/assemble.mjs` rewrites it to the
exact published version when releasing. Build the runtime first:

```sh
(cd typedpg_npm && npm ci && npm run build)
(cd typedpg_npm_cli && npm ci && npm test)
```

## License

MIT.
