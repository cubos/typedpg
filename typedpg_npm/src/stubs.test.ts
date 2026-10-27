import assert from "node:assert/strict";
import { test } from "node:test";

import { moved } from "./moved.js";

// The bundler plugins moved to @cubos/typedpg-cli in 0.6.0: a project that
// upgrades must be told where they went, not that the subpath disappeared.

for (const plugin of ["esbuild", "vite", "rollup"]) {
  test(`@cubos/typedpg/${plugin} says where it moved`, async () => {
    await assert.rejects(import(`./${plugin}.js`), { message: moved(plugin).message });
  });
}

test("@cubos/typedpg/webpack says where it moved", async () => {
  const { default: loader } = (await import("./webpack.js")) as { default: () => never };
  assert.throws(loader, { message: moved("webpack").message });
});
