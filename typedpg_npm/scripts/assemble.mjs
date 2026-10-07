#!/usr/bin/env node
// Assemble the npm packages for a release: `typedpg` itself (built, with
// an optional dependency on each platform package) and one
// `@typedpg/cli-<target>` per binary.
//
//   node scripts/assemble.mjs <binaries-dir> <out-dir>
//
// <binaries-dir> holds `<target>/typedpg` (`typedpg.exe` on Windows), the
// targets named as bin/typedpg.js's target() does: linux-x64-gnu,
// linux-arm64-musl, darwin-arm64, win32-x64-msvc, … Each <out-dir>/<name>
// is then ready for `npm publish`.

import { chmodSync, cpSync, existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const [binaries, out] = process.argv.slice(2);
if (!binaries || !out) {
  console.error("usage: assemble.mjs <binaries-dir> <out-dir>");
  process.exit(2);
}
const pkgDir = dirname(dirname(fileURLToPath(import.meta.url)));
const main = JSON.parse(readFileSync(join(pkgDir, "package.json"), "utf8"));
if (!existsSync(join(pkgDir, "dist", "index.js"))) {
  console.error("assemble.mjs: build the package first (npm run build)");
  process.exit(1);
}

rmSync(out, { recursive: true, force: true });
const optional = {};
for (const target of readdirSync(binaries).sort()) {
  const [os, cpu, libc] = target.split("-");
  const file = os === "win32" ? "typedpg.exe" : "typedpg";
  const name = `@typedpg/cli-${target}`;
  const dir = join(out, `cli-${target}`);
  mkdirSync(join(dir, "bin"), { recursive: true });
  cpSync(join(binaries, target, file), join(dir, "bin", file));
  chmodSync(join(dir, "bin", file), 0o755);
  const pkg = {
    name,
    version: main.version,
    description: `The typedpg binary for ${target}`,
    license: main.license,
    repository: main.repository,
    os: [os],
    cpu: [cpu],
    ...(os === "linux" ? { libc: [libc === "musl" ? "musl" : "glibc"] } : {}),
    files: ["bin"],
    preferUnplugged: true,
  };
  writeFileSync(join(dir, "package.json"), JSON.stringify(pkg, null, 2) + "\n");
  optional[name] = main.version;
}

const dir = join(out, "typedpg");
mkdirSync(dir, { recursive: true });
for (const entry of ["dist", "bin", "README.md", "LICENSE-MIT", "LICENSE-APACHE"]) {
  const from = ["LICENSE-MIT", "LICENSE-APACHE"].includes(entry) ? join(pkgDir, "..", entry) : join(pkgDir, entry);
  if (existsSync(from)) cpSync(from, join(dir, entry), { recursive: true, filter: (p) => !/\.test\.(js|d\.ts)$/.test(p) });
}
const { devDependencies: _, scripts: __, ...published } = main;
const files = [...published.files, "LICENSE-MIT", "LICENSE-APACHE"];
writeFileSync(join(dir, "package.json"), JSON.stringify({ ...published, files, optionalDependencies: optional }, null, 2) + "\n");
console.log(`assembled typedpg ${main.version} with ${Object.keys(optional).join(", ") || "no binaries"} in ${out}`);
