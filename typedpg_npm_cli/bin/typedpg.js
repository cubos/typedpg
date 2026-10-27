#!/usr/bin/env node
// The `typedpg` command: runs the native binary of this platform, which
// npm installed as one of this package's optional dependencies
// (`@cubos/typedpg-cli-<platform>`). TYPEDPG_BINARY overrides it.

import { spawnSync } from "node:child_process";
import { binary } from "../dist/binary.js";

let path;
try {
  path = binary();
} catch (e) {
  console.error(e.message);
  process.exit(1);
}
const result = spawnSync(path, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  console.error(`typedpg: ${result.error.message}`);
  process.exit(1);
}
if (result.signal) process.kill(process.pid, result.signal);
process.exit(result.status ?? 1);
