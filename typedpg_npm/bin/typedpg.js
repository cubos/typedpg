#!/usr/bin/env node
// The `typedpg` command: runs the native binary of this platform, which
// npm installed as one of this package's optional dependencies
// (`@cubos/typedpg-cli-<platform>`). TYPEDPG_BINARY overrides it.

import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);

/** This platform's package name suffix, as scripts/assemble.mjs names them. */
function target() {
  const { platform, arch } = process;
  if (platform === "linux") {
    // glibc's runtime version is in the process report; musl has none.
    const glibc = process.report?.getReport?.().header?.glibcVersionRuntime;
    return `linux-${arch}-${glibc ? "gnu" : "musl"}`;
  }
  if (platform === "win32") return `win32-${arch}-msvc`;
  return `${platform}-${arch}`;
}

function binary() {
  if (process.env.TYPEDPG_BINARY) return process.env.TYPEDPG_BINARY;
  const pkg = `@cubos/typedpg-cli-${target()}`;
  const file = process.platform === "win32" ? "typedpg.exe" : "typedpg";
  try {
    return require.resolve(`${pkg}/bin/${file}`);
  } catch {
    console.error(
      `typedpg: no binary for this platform (${target()}): the optional dependency ${pkg} is not ` +
        `installed. Reinstall without --no-optional / --omit=optional, or set TYPEDPG_BINARY.`,
    );
    process.exit(1);
  }
}

const result = spawnSync(binary(), process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  console.error(`typedpg: ${result.error.message}`);
  process.exit(1);
}
if (result.signal) process.kill(process.pid, result.signal);
process.exit(result.status ?? 1);
