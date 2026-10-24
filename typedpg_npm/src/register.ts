// `node --import @cubos/typedpg/register app.ts`: importing a `.sql` file
// loads its module, from the cache `typedpg build` fills (see sqlfiles.ts).
// A synchronous hook (`module.registerHooks`, Node 22.15+), so `require()`
// of a `.sql` file works too.

import * as nodeModule from "node:module";
import { fileURLToPath } from "node:url";
import { compileSql } from "./sqlfiles.js";

if (typeof nodeModule.registerHooks !== "function") {
  throw new Error(`@cubos/typedpg/register needs module.registerHooks: Node 22.15 or later (this is ${process.version})`);
}

nodeModule.registerHooks({
  load(url, context, nextLoad) {
    const path = url.split(/[?#]/, 1)[0]!;
    if (path.startsWith("file:") && path.endsWith(".sql")) {
      return { format: "module", source: compileSql(fileURLToPath(path)).code, shortCircuit: true };
    }
    return nextLoad(url, context);
  },
});
