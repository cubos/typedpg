// Bun: importing a `.sql` file loads its module, from the cache `typedpg
// build` fills (see sqlfiles.ts). As a preload it registers itself for the
// runtime (`bunfig.toml`: `preload = ["@cubos/typedpg/bun"]`); its default
// export is the plugin, for `Bun.build({ plugins: [typedpg] })`.

import { compileSql } from "./sqlfiles.js";

interface BunPluginBuild {
  onLoad(
    options: { filter: RegExp },
    callback: (args: { path: string }) => { contents: string; loader: "js" },
  ): void;
}

export interface BunPlugin {
  name: string;
  setup(build: BunPluginBuild): void;
}

const typedpg: BunPlugin = {
  name: "typedpg",
  setup(build) {
    build.onLoad({ filter: /\.sql$/ }, ({ path }) => ({ contents: compileSql(path).code, loader: "js" }));
  },
};

(globalThis as { Bun?: { plugin(plugin: BunPlugin): void } }).Bun?.plugin(typedpg);

export default typedpg;
