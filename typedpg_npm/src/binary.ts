// Where the `typedpg` native binary of this platform is: npm installs it as
// one of this package's optional dependencies (`@cubos/typedpg-cli-<platform>`).
// TYPEDPG_BINARY overrides it.

import { createRequire } from "node:module";

const require = createRequire(import.meta.url);

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

/** The binary's path; throws, saying why, when there is none. */
export function binary(): string {
  if (process.env.TYPEDPG_BINARY) return process.env.TYPEDPG_BINARY;
  const pkg = `@cubos/typedpg-cli-${target()}`;
  const file = process.platform === "win32" ? "typedpg.exe" : "typedpg";
  try {
    return require.resolve(`${pkg}/bin/${file}`);
  } catch {
    throw new Error(
      `typedpg: no binary for this platform (${target()}): the optional dependency ${pkg} is not ` +
        `installed. Reinstall without --no-optional / --omit=optional, or set TYPEDPG_BINARY.`,
    );
  }
}
