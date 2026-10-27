// For `@cubos/typedpg-cli` only, which builds the modules of `.sql` files
// with the cache and binary lookup below: not a public API, and free to
// change with it.

export { binary, target } from "./binary.js";
export { compileSql } from "./sqlfiles.js";
export type { CompiledSql } from "./sqlfiles.js";
