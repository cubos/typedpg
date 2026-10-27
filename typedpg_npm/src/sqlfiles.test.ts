// The loaders' compilation path: `typedpg serve` kept alive per binary,
// respawned after an idle exit, retried once on a crash, and never keeping
// the host process alive.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { test } from "node:test";
import { pathToFileURL } from "node:url";

import { compileSqlAsync } from "./sqlfiles.js";

// A stand-in for the binary: speaks the same `Content-Length` framing,
// answers `typedpg/compile`, and logs each spawn and request. Modes:
// "respond" (stay alive), "exit-after-response" (an idle timeout, at once)
// and "crash-once" (the first child dies with a request in flight).
const FAKE = `#!/usr/bin/env node
const fs = require("node:fs");
const log = process.env.TYPEDPG_FAKE_LOG;
const mode = process.env.TYPEDPG_FAKE_MODE || "respond";
const spawns = () => {
  try {
    return fs.readFileSync(log, "utf8").split("\\n").filter((l) => l.startsWith("spawn ")).length;
  } catch {
    return 0;
  }
};
const index = spawns() + 1;
fs.appendFileSync(log, \`spawn \${process.pid}\\n\`);
let buffer = Buffer.alloc(0);
process.stdin.on("data", (chunk) => {
  buffer = Buffer.concat([buffer, chunk]);
  for (;;) {
    const end = buffer.indexOf("\\r\\n\\r\\n");
    if (end < 0) return;
    const header = buffer.subarray(0, end).toString("ascii");
    const length = Number(/content-length:\\s*(\\d+)/i.exec(header)[1]);
    const start = end + 4;
    if (buffer.length < start + length) return;
    const message = JSON.parse(buffer.subarray(start, start + length).toString("utf8"));
    buffer = buffer.subarray(start + length);
    fs.appendFileSync(log, \`request \${message.id}\\n\`);
    if (mode === "crash-once" && index === 1) {
      fs.appendFileSync(log, "crash\\n");
      process.exit(1);
    }
    const body = JSON.stringify({
      jsonrpc: "2.0",
      id: message.id,
      result: { code: \`export const served = \${index};\\n\`, dependencies: [], diagnostics: [] },
    });
    fs.writeSync(1, \`Content-Length: \${Buffer.byteLength(body)}\\r\\n\\r\\n\${body}\`);
    if (mode === "exit-after-response") process.exit(0);
  }
});
`;

function write(path: string, text: string): void {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, text);
}

function project(): { root: string; sql: string } {
  const root = mkdtempSync(join(tmpdir(), "typedpg-sqlfiles-"));
  write(
    join(root, "typedpg.config.json"),
    JSON.stringify({ sql: { include: ["q/*.sql"], emit: "none" } }),
  );
  write(
    join(root, "migrations/0001.sql"),
    "CREATE TABLE users (id int4 PRIMARY KEY, name text NOT NULL);",
  );
  const sql = join(root, "q/users.sql");
  write(sql, "/* @name byId */\nSELECT name FROM users WHERE id = $id;\n");
  return { root, sql };
}

function fakeBinary(root: string): string {
  const path = join(root, "fake-typedpg");
  write(path, FAKE);
  chmodSync(path, 0o755);
  return path;
}

async function withEnv<T>(env: Record<string, string>, run: () => Promise<T>): Promise<T> {
  const saved = new Map(Object.entries(env).map(([key]) => [key, process.env[key]]));
  Object.assign(process.env, env);
  try {
    return await run();
  } finally {
    for (const [key, value] of saved) {
      if (value === undefined) delete process.env[key];
      else process.env[key] = value;
    }
  }
}

function lines(log: string): string[] {
  try {
    return readFileSync(log, "utf8").split("\n").filter((line) => line !== "");
  } catch {
    return [];
  }
}

function count(log: string, prefix: string): number {
  return lines(log).filter((line) => line.startsWith(prefix)).length;
}

function environment(root: string, log: string, mode: string): Record<string, string> {
  return {
    TYPEDPG_BINARY: fakeBinary(root),
    TYPEDPG_CACHE_DIR: join(root, "cache"),
    TYPEDPG_SERVE_IDLE_TIMEOUT: "30",
    TYPEDPG_FAKE_LOG: log,
    TYPEDPG_FAKE_MODE: mode,
  };
}

test("two cache misses reuse one serve child", async () => {
  const { root, sql } = project();
  const log = join(root, "fake.log");
  await withEnv(environment(root, log, "respond"), async () => {
    const first = await compileSqlAsync(sql);
    const second = await compileSqlAsync(sql);
    // The same child answered both (misses, as the fake fills no cache).
    assert.match(first.code, /export const served = 1;/);
    assert.match(second.code, /export const served = 1;/);
    assert.equal(count(log, "spawn "), 1);
    assert.equal(count(log, "request "), 2);
  });
});

test("a child that exits is respawned for the next miss", async () => {
  const { root, sql } = project();
  const log = join(root, "fake.log");
  await withEnv(environment(root, log, "exit-after-response"), async () => {
    assert.match((await compileSqlAsync(sql)).code, /export const served = 1;/);
    // Let the exit reach its parent, as an idle timeout would.
    await delay(100);
    assert.match((await compileSqlAsync(sql)).code, /export const served = 2;/);
    assert.equal(count(log, "spawn "), 2);
  });
});

test("a crash with a request in flight retries once on a fresh child", async () => {
  const { root, sql } = project();
  const log = join(root, "fake.log");
  await withEnv(environment(root, log, "crash-once"), async () => {
    const compiled = await compileSqlAsync(sql);
    assert.match(compiled.code, /export const served = 2;/);
    assert.equal(count(log, "crash"), 1);
    assert.equal(count(log, "spawn "), 2);
  });
});

test("the host exits while its serve child is still alive", async () => {
  const { root, sql } = project();
  const log = join(root, "fake.log");
  const env = environment(root, log, "respond");
  const host = join(root, "host.mjs");
  write(
    host,
    `import { compileSqlAsync } from ${JSON.stringify(
      pathToFileURL(join(import.meta.dirname, "sqlfiles.js")).href,
    )};\nconst compiled = await compileSqlAsync(${JSON.stringify(sql)});\nconsole.log(compiled.code);\n`,
  );
  // The fake child would live for 30 s: the host must not wait for it.
  const result = spawnSync(process.execPath, [host], {
    encoding: "utf8",
    env: { ...process.env, ...env },
    timeout: 15_000,
  });
  assert.equal(result.status, 0, result.stderr || result.error?.message);
  assert.match(result.stdout, /export const served = 1;/);
  assert.equal(count(log, "spawn "), 1);
});
