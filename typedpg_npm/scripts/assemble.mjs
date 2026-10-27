#!/usr/bin/env node
// Assemble the npm packages for a release: `@cubos/typedpg` (the runtime),
// `@cubos/typedpg-cli` (the `typedpg` command and the bundler plugins) and
// one `@cubos/typedpg-cli-<target>` per binary.
//
//   node scripts/assemble.mjs <binaries-dir> <out-dir>
//
// <binaries-dir> holds `<target>/typedpg` (`typedpg.exe` on Windows), the
// targets named as src/binary.ts's target() does: linux-x64-gnu,
// linux-arm64-musl, darwin-arm64, win32-x64-msvc, … Each <out-dir>/<name>
// is then ready for `npm publish`.
//
// The runtime carries no binary and no bin: an application installs it as a
// production dependency. The CLI is the devDependency: the platform packages
// become its optional dependencies, and its dependency on the runtime is the
// exact version both are published at (its package.json names the runtime by
// path in the repository, so the two build together).

import { chmodSync, cpSync, existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const [binaries, out] = process.argv.slice(2);
if (!binaries || !out) {
  console.error("usage: assemble.mjs <binaries-dir> <out-dir>");
  process.exit(2);
}
const pkgDir = dirname(dirname(fileURLToPath(import.meta.url)));
const repo = dirname(pkgDir);
const cliDir = join(repo, "typedpg_npm_cli");
const main = JSON.parse(readFileSync(join(pkgDir, "package.json"), "utf8"));
const cli = JSON.parse(readFileSync(join(cliDir, "package.json"), "utf8"));

for (const [name, dir, file] of [
  [main.name, pkgDir, "index.js"],
  [cli.name, cliDir, "esbuild.js"],
]) {
  if (!existsSync(join(dir, "dist", file))) {
    console.error(`assemble.mjs: build ${name} first (npm run build in ${dir})`);
    process.exit(1);
  }
}
if (cli.version !== main.version) {
  console.error(`assemble.mjs: ${main.name} is ${main.version} but ${cli.name} is ${cli.version}`);
  process.exit(1);
}

rmSync(out, { recursive: true, force: true });

/** Copy a package's build, README and the repository's licenses into `to`. */
function copyInto(from, to, entries) {
  for (const entry of entries) {
    const src = entry === "LICENSE" ? join(repo, entry) : join(from, entry);
    if (existsSync(src)) cpSync(src, join(to, entry), { recursive: true, filter: (p) => !/\.test\.(js|d\.ts)$/.test(p) });
  }
}

const optional = {};
for (const target of readdirSync(binaries).sort()) {
  const [os, cpu, libc] = target.split("-");
  const file = os === "win32" ? "typedpg.exe" : "typedpg";
  const name = `@cubos/typedpg-cli-${target}`;
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

// The runtime: the query runtime, codecs, adapters, migration runner and the
// Node and Bun loaders — no binary, and no bin.
{
  const dir = join(out, "typedpg");
  mkdirSync(dir, { recursive: true });
  copyInto(pkgDir, dir, ["dist", "README.md", "LICENSE"]);
  const { devDependencies: _, scripts: __, ...published } = main;
  const files = [...published.files, "LICENSE"];
  writeFileSync(join(dir, "package.json"), JSON.stringify({ ...published, files }, null, 2) + "\n");
}

// The CLI: the command, the bundler plugins, the platform packages as its
// optional dependencies and the runtime at the version being released.
{
  const dir = join(out, "typedpg-cli");
  mkdirSync(dir, { recursive: true });
  copyInto(cliDir, dir, ["dist", "bin", "README.md", "LICENSE"]);
  const { devDependencies: _, scripts: __, ...published } = cli;
  const dependencies = { ...published.dependencies, [main.name]: main.version };
  const files = [...published.files, "LICENSE"];
  writeFileSync(
    join(dir, "package.json"),
    JSON.stringify({ ...published, dependencies, files, optionalDependencies: optional }, null, 2) + "\n",
  );
}

console.log(
  `assembled ${main.name} ${main.version} and ${cli.name} ` +
    `with ${Object.keys(optional).join(", ") || "no binaries"} in ${out}`,
);
