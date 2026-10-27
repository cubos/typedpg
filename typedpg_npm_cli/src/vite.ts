// Vite, Rollup and Rolldown (and vitest): `plugins: [typedpg()]` loads the
// module of each `.sql` file imported, from the cache `typedpg build` fills
// (`@cubos/typedpg`'s sqlfiles.ts, reached through its internal API); the
// config file and the migrations are watched. A cache miss compiles through
// the persistent `typedpg serve` dev servers keep around.

import { compileSqlAsync } from "@cubos/typedpg/internal";

interface PluginContext {
  addWatchFile(file: string): void;
}

export interface VitePlugin {
  name: string;
  enforce: "pre";
  load(this: PluginContext, id: string): Promise<string | null>;
}

export default function typedpg(): VitePlugin {
  return {
    name: "typedpg",
    enforce: "pre",
    async load(id) {
      // `\0`: a virtual module; `?raw` and other queries: not the module.
      if (id.startsWith("\0") || id.includes("?") || !id.endsWith(".sql")) return null;
      const compiled = await compileSqlAsync(id);
      for (const file of compiled.dependencies) this.addWatchFile(file);
      return compiled.code;
    },
  };
}
