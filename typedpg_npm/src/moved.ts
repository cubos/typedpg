// The bundler plugins moved out of the runtime into `@cubos/typedpg-cli` in
// 0.6.0. The old subpaths stay as stubs so that an upgrade fails saying
// where they went, not with "Cannot find module".

/** The error of a `@cubos/typedpg/<plugin>` subpath that moved. */
export function moved(plugin: string): Error {
  return new Error(
    `@cubos/typedpg/${plugin} moved to @cubos/typedpg-cli/${plugin} in 0.6.0: ` +
      `install @cubos/typedpg-cli as a devDependency and import it from there`,
  );
}
