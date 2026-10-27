// webpack and rspack: the loader of `.sql` files, which gives each one's
// module from the cache `typedpg build` fills (`@cubos/typedpg`'s
// sqlfiles.ts, reached through its internal API):
//
//   module: { rules: [{ test: /\.sql$/, loader: "@cubos/typedpg-cli/webpack", type: "javascript/esm" }] }
//
// A cache miss compiles through the persistent `typedpg serve` watch mode
// keeps around, so the loader is asynchronous (`this.async()`).

import { compileSqlAsync } from "@cubos/typedpg/internal";

interface LoaderContext {
  resourcePath: string;
  addDependency(file: string): void;
  async(): (error: Error | null, result?: string) => void;
}

export default function typedpgLoader(this: LoaderContext): void {
  const callback = this.async();
  compileSqlAsync(this.resourcePath).then(
    (compiled) => {
      for (const file of compiled.dependencies) this.addDependency(file);
      callback(null, compiled.code);
    },
    (error: Error) => callback(error),
  );
}
