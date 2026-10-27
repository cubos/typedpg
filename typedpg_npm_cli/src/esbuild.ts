// esbuild: `plugins: [typedpg()]` bundles the module of each `.sql` file
// imported, from the cache `typedpg build` fills (`@cubos/typedpg`'s
// sqlfiles.ts, reached through its internal API). A cache miss compiles
// through the persistent `typedpg serve` watch mode keeps around.

import { dirname } from "node:path";
import { compileSqlAsync } from "@cubos/typedpg/internal";

interface EsbuildPluginBuild {
  onLoad(
    options: { filter: RegExp; namespace?: string },
    callback: (args: { path: string }) => Promise<{
      contents: string;
      loader: "js";
      resolveDir: string;
      watchFiles: string[];
    }>,
  ): void;
}

export interface EsbuildPlugin {
  name: string;
  setup(build: EsbuildPluginBuild): void;
}

export default function typedpg(): EsbuildPlugin {
  return {
    name: "typedpg",
    setup(build) {
      build.onLoad({ filter: /\.sql$/, namespace: "file" }, async ({ path }) => {
        const compiled = await compileSqlAsync(path);
        return {
          contents: compiled.code,
          loader: "js",
          resolveDir: dirname(path),
          watchFiles: compiled.dependencies,
        };
      });
    },
  };
}
