// webpack and rspack: the loader of `.sql` files, which gives each one's
// module from the cache `typedpg build` fills (`@cubos/typedpg`'s
// sqlfiles.ts, reached through its internal API):
//
//   module: { rules: [{ test: /\.sql$/, loader: "@cubos/typedpg-cli/webpack", type: "javascript/esm" }] }

import { compileSql } from "@cubos/typedpg/internal";

interface LoaderContext {
  resourcePath: string;
  addDependency(file: string): void;
}

export default function typedpgLoader(this: LoaderContext): string {
  const compiled = compileSql(this.resourcePath);
  for (const file of compiled.dependencies) this.addDependency(file);
  return compiled.code;
}
