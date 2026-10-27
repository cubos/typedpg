// Where this platform's `typedpg` native binary is, when the install has
// one: `TYPEDPG_BINARY`, or the binary of `@cubos/typedpg-cli` — a
// devDependency — whose optional dependencies are the platform packages
// (`@cubos/typedpg-cli-<platform>`). A production install has neither: the
// cache `typedpg build` filled at image build time needs no binary.
//
// The CLI is resolved from the project's config file, not from this package:
// a package manager may install the runtime and the CLI under different
// `node_modules`, and only the project's own has both.

import { createRequire } from "node:module";

/** This platform's package name suffix, as scripts/assemble.mjs names them. */
export function target(): string {
  const { platform, arch } = process;
  if (platform === "linux") {
    // glibc's runtime version is in the process report; musl has none.
    const report = process.report?.getReport?.() as { header?: { glibcVersionRuntime?: string } } | undefined;
    return `linux-${arch}-${report?.header?.glibcVersionRuntime ? "gnu" : "musl"}`;
  }
  if (platform === "win32") return `win32-${arch}-msvc`;
  return `${platform}-${arch}`;
}

/**
 * The binary's path, or `null` when this install has none. `anchor` is a
 * file of the project — its config file — where resolving
 * `@cubos/typedpg-cli` starts.
 */
export function binary(anchor: string): string | null {
  if (process.env.TYPEDPG_BINARY) return process.env.TYPEDPG_BINARY;
  const pkg = `@cubos/typedpg-cli-${target()}`;
  const file = process.platform === "win32" ? "typedpg.exe" : "typedpg";
  try {
    // The platform packages are the CLI's optional dependencies: resolve
    // the CLI first, then the platform package from where it installed it
    // (a package manager need not hoist it to the project's node_modules).
    const cli = createRequire(anchor).resolve("@cubos/typedpg-cli/package.json");
    return createRequire(cli).resolve(`${pkg}/bin/${file}`);
  } catch {
    return null;
  }
}
