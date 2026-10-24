// Bundles main.ts and its .sql file with Rollup (esbuild strips the types).
import { transform } from "esbuild";
import typedpg from "@cubos/typedpg/rollup";

const typescript = {
  name: "typescript",
  async transform(code, id) {
    if (id.endsWith(".ts")) return (await transform(code, { loader: "ts" })).code;
    return null;
  },
};

export default {
  input: "src/main.ts",
  output: { file: "out/rollup.mjs", format: "es" },
  external: (id) => !id.startsWith(".") && !id.startsWith("/"),
  plugins: [typedpg(), typescript],
};
