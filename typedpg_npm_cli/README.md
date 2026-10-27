# typedpg for TypeScript: the command and bundler plugins

The development package of
[`@cubos/typedpg`](https://www.npmjs.com/package/@cubos/typedpg), the
TypeScript runtime. It gives you the `typedpg` command, the bundler plugins
of `.sql` files, and the TypeScript 7.1 content mapper.

```sh
npm install @cubos/typedpg
npm install --save-dev @cubos/typedpg-cli
```

The command runs a native binary, installed for your platform as an optional
dependency (`@cubos/typedpg-cli-linux-x64-gnu`, `-darwin-arm64`, …), so a
production install of `@cubos/typedpg` never downloads it. Node.js 22.12 or
later.

## Commands

`typedpg <command>`; for the commands that read it, the config defaults to
`typedpg.config.json` in the current directory.

| Command | |
|---|---|
| `typedpg gen [--watch]` | Generate each database's module from the queries in the sources. `--watch` keeps running, regenerating as sources and migrations change. |
| `typedpg check` | Fail if a generated module is out of date, or a query has an error. |
| `typedpg build` | Compile the `.sql` files `sql.include` names into the cache the loaders and bundler plugins read, so importing one runs no analysis. `--json` prints the outcome as JSON on stdout, which the loaders read. |
| `typedpg serve [--idle-timeout <seconds>]` | Serve TypeScript's content mapper and the loaders over JSON-RPC on stdin/stdout. `--idle-timeout` makes it exit after that many seconds without a request; by default it does not exit. |
| `typedpg migrate up` | Apply all pending migrations. |
| `typedpg migrate status` | Show each migration's status. |
| `typedpg migrate down [name] [--force]` | Revert a migration (the last applied one by default). `--force` reverts even without a `.down.sql` file. |
| `typedpg migrate accept <name>` | Record an applied migration's edited file as applied, without running it. |
| `typedpg migrate create <name>` | Create an empty `<timestamp>_<name>.sql` and `<timestamp>_<name>.down.sql`. It needs no database. |

`gen`, `check` and `build` take `--config <path>`. `migrate` takes
`--config`, `--db <name>` (when the config has several databases) and
`--url <url>` (the connection URL, defaulting to `DATABASE_URL`; a `.env`
file is read). The migrate actions other than `create` need a connection.

In CI, `npx typedpg check` exits non-zero when a generated module is stale,
when a query has an error, or when a module remains for a `.sql` file that is
gone.

## Bundler plugins

Each plugin loads the module of an imported `.sql` file from the cache
`typedpg build` fills.

| Bundler | |
|---|---|
| esbuild | `import typedpg from "@cubos/typedpg-cli/esbuild"`, then `plugins: [typedpg()]` |
| Vite, vitest, Rollup, Rolldown | `import typedpg from "@cubos/typedpg-cli/vite"` (or `/rollup`), then `plugins: [typedpg()]` |
| webpack, rspack | `{ test: /\.sql$/, loader: "@cubos/typedpg-cli/webpack", type: "javascript/esm" }` |

On a cache miss, the plugins compile asynchronously through a persistent
`typedpg serve`, one child per binary: it keeps the project's schema in
memory between compilations and exits after five minutes without a request
(`TYPEDPG_SERVE_IDLE_TIMEOUT`, in seconds); the next cache miss spawns
another. The Node hook (`@cubos/typedpg/register`) is synchronous and runs
`typedpg build` once instead. A cache hit never needs a binary.

## TypeScript 7.1 content mapper

TypeScript 7.1 gets a `.sql` file's types from this package: it runs
`typedpg serve` as the file's content mapper, so no module has to be
written (`emit: "none"`). The CLI must be a devDependency:

```json
{
  "contentMappers": [{ "package": "@cubos/typedpg-cli", "extensions": [".sql"] }]
}
```

`tsc --runExternalCode` then checks the `.sql` files too, each error at its
place in the file. See
[The content mapper](https://github.com/cubos/typedpg/blob/main/typedpg_npm/README.md#the-content-mapper-typescript-71)
in the runtime's README.

## Environment variables

| Variable | |
|---|---|
| `DATABASE_URL` | The connection URL for `typedpg migrate`, when `--url` is not given. A `.env` file is read. |
| `TYPEDPG_BINARY` | Run this binary instead of the platform package. |
| `TYPEDPG_CACHE_DIR` | The `.sql` cache directory. Default: `node_modules/.cache/typedpg` beside the config file. |
| `TYPEDPG_SERVE_IDLE_TIMEOUT` | Seconds a `typedpg serve` spawned by the loaders or plugins lives without a request. Default: 300. |

## Platforms

| OS | Architectures |
|---|---|
| Linux (glibc) | `x64`, `arm64` |
| Linux (musl) | `x64`, `arm64` |
| macOS | `x64`, `arm64` |
| Windows | `x64` (`win32-x64-msvc`) |

Each is a package of its own (`@cubos/typedpg-cli-linux-x64-gnu`,
`@cubos/typedpg-cli-linux-arm64-musl`, `@cubos/typedpg-cli-darwin-x64`,
`@cubos/typedpg-cli-darwin-arm64`, `@cubos/typedpg-cli-win32-x64-msvc`, …),
installed as an optional dependency of `@cubos/typedpg-cli`. An install that
skips optional dependencies (`--omit=optional`, `--no-optional`) has no
binary: reinstall with them, or set `TYPEDPG_BINARY`.

On another platform, build one from source and point `TYPEDPG_BINARY` at
it:

```sh
git submodule update --init    # the parser, libpg_query, is a submodule
cargo build --release -p typedpg_ts --bin typedpg
```

## License

MIT.
