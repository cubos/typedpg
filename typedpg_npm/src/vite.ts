// Vite, Rollup and Rolldown (and vitest): `plugins: [typedpg()]` loads the
// module of each `.sql` file imported, from the cache `typedpg build` fills
// (see sqlfiles.ts); the config file and the migrations are watched.

import { compileSql } from "./sqlfiles.js";

interface PluginContext {
  addWatchFile(file: string): void;
}

export interface VitePlugin {
  name: string;
  enforce: "pre";
  load(this: PluginContext, id: string): string | null;
}

export default function typedpg(): VitePlugin {
  return {
    name: "typedpg",
    enforce: "pre",
    load(id) {
      // `\0`: a virtual module; `?raw` and other queries: not the module.
      if (id.startsWith("\0") || id.includes("?") || !id.endsWith(".sql")) return null;
      const compiled = compileSql(id);
      for (const file of compiled.dependencies) this.addWatchFile(file);
      return compiled.code;
    },
  };
}
