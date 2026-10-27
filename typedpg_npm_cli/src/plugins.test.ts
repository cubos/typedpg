import assert from "node:assert/strict";
import { test } from "node:test";

// Each subpath must resolve through the package's exports map, and importing
// it must reach the runtime's internal API (`compileSql` through
// `@cubos/typedpg/internal`): a missing export fails here, not in a user's
// build. The specifier is built at runtime, as the package's dist/ does not
// exist yet while this file compiles.
const pkg = "@cubos/typedpg-cli";

for (const plugin of ["esbuild", "vite", "rollup"]) {
  test(`./${plugin} is its plugin`, async () => {
    const module = (await import(`${pkg}/${plugin}`)) as { default: () => { name: string; enforce?: string } };
    const instance = module.default();
    assert.equal(instance.name, "typedpg");
    if (plugin !== "esbuild") assert.equal(instance.enforce, "pre");
  });
}

test("./webpack is its loader", async () => {
  const module = (await import(`${pkg}/webpack`)) as { default: unknown };
  assert.equal(typeof module.default, "function");
});
