// Where this platform's `typedpg` native binary is: npm installs it as one
// of this package's optional dependencies (`@cubos/typedpg-cli-<platform>`).
// TYPEDPG_BINARY overrides it.

import { createRequire } from "node:module";

import { target } from "@cubos/typedpg/internal";

const require = createRequire(import.meta.url);

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
