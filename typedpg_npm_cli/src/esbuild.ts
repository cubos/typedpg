// esbuild: `plugins: [typedpg()]` bundles the module of each `.sql` file
// imported, from the cache `typedpg build` fills (`@cubos/typedpg`'s
// sqlfiles.ts, reached through its internal API).

import { dirname } from "node:path";
import { compileSql } from "@cubos/typedpg/internal";

interface EsbuildPluginBuild {
  onLoad(
    options: { filter: RegExp; namespace?: string },
    callback: (args: { path: string }) => { contents: string; loader: "js"; resolveDir: string; watchFiles: string[] },
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
      build.onLoad({ filter: /\.sql$/, namespace: "file" }, ({ path }) => {
        const compiled = compileSql(path);
        return { contents: compiled.code, loader: "js", resolveDir: dirname(path), watchFiles: compiled.dependencies };
      });
    },
  };
}
