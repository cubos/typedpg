// Bundles main.ts and its .sql file with esbuild.
import { build } from "esbuild";
import typedpg from "@cubos/typedpg-cli/esbuild";

await build({
  entryPoints: ["src/main.ts"],
  outfile: "out/esbuild.mjs",
  bundle: true,
  platform: "node",
  format: "esm",
  packages: "external",
  plugins: [typedpg()],
});
