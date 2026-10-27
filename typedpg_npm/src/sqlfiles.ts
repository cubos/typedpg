// What the loaders and bundler plugins share: the ES module of a `.sql`
// file, read from the cache `typedpg build` fills.
//
// The cache is keyed by everything a module depends on — the typedpg
// version, the config file, every database's migrations, the file's path and
// content — computed here as the binary computes it (typedpg_ts/src/cache.rs:
// change the recipe in both). A file with no entry compiles on demand when
// the install has a binary, which only a devDependency on
// `@cubos/typedpg-cli` gives it: `compileSql` runs `typedpg build` once
// (the Node hook is synchronous), `compileSqlAsync` sends the file to a
// persistent `typedpg serve` per binary, which keeps the schema in memory
// between compilations and exits after a few minutes idle. A warm cache
// (`typedpg build` in the image's build) needs no binary at all.

import { execFileSync, spawn, type ChildProcess } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readFileSync, readdirSync, realpathSync, statSync } from "node:fs";
import { dirname, join, relative, resolve, sep } from "node:path";
import type { Readable, Writable } from "node:stream";
import { binary } from "./binary.js";

const FORMAT = "typedpg-cache-1";
const CONFIG_FILE = "typedpg.config.json";
/** How long a serve child lives without a request, in seconds, unless
 * `TYPEDPG_SERVE_IDLE_TIMEOUT` says otherwise. */
const DEFAULT_IDLE_TIMEOUT = 300;

const VERSION: string = (
  JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8")) as { version: string }
).version;

/** A compiled `.sql` file: its module, and the files it depends on (to watch). */
export interface CompiledSql {
  code: string;
  dependencies: string[];
}

/** A problem in a `.sql` file, as `typedpg build --json` reports it. */
interface Diagnostic {
  file: string;
  line: number | null;
  col: number | null;
  message: string;
}

interface Project {
  configFile: string;
  root: string;
  identity: string;
  /** The config file and every migration, to watch. */
  files: string[];
  /** Their sizes and modification times: when they change, the identity is computed again. */
  signature: string;
}

/** Everything a compile needs, from the cache key to the error messages. */
interface Lookup {
  /** The config file as it was found (it may be relative). */
  configFile: string;
  /** The config file for the binary, which takes absolute paths. */
  absoluteConfig: string;
  /** The `.sql` file for the binary. */
  absoluteFile: string;
  /** The `.sql` file relative to the project, as errors name it. */
  rel: string;
  entry: string;
  dependencies: string[];
}

const projects = new Map<string, Project>();
const configs = new Map<string, string | null>();

/** The module of the `.sql` file at `file` (an absolute path). */
export function compileSql(file: string): CompiledSql {
  const lookup = prepare(file);
  if (existsSync(lookup.entry)) return cached(lookup);
  const bin = requireBinary(lookup);
  const report = build(bin, lookup.configFile);
  if (existsSync(lookup.entry)) return cached(lookup);
  throw errorOf(lookup, report.diagnostics) ?? notCompiled(lookup);
}

/**
 * The module of the `.sql` file at `file`, compiled by a persistent
 * `typedpg serve` when no cache entry exists. The child is lazy, shared per
 * binary and unref'd: it compiles the next cache miss and never keeps the
 * host alive by itself.
 */
export async function compileSqlAsync(file: string): Promise<CompiledSql> {
  const lookup = prepare(file);
  if (existsSync(lookup.entry)) return cached(lookup);
  const bin = requireBinary(lookup);
  let result: CompileResult;
  try {
    result = await serveCompile(bin, lookup.absoluteConfig, lookup.absoluteFile);
  } catch {
    // The serve child crashed twice, or answered with an error: compile the
    // way the synchronous path does, and report its error.
    return settle(lookup, build(bin, lookup.configFile));
  }
  if (existsSync(lookup.entry)) return cached(lookup);
  const error = errorOf(lookup, result.diagnostics);
  if (error !== null) throw error;
  // A server that compiled the file may not have managed to write the cache
  // (a stale directory, a read-only tree): its module is still the answer.
  if (result.code !== null) return { code: result.code, dependencies: lookup.dependencies };
  throw notCompiled(lookup);
}

function prepare(file: string): Lookup {
  const configFile = findConfig(dirname(file));
  if (configFile === null) {
    throw new Error(`typedpg: no ${CONFIG_FILE} in the directory of ${file} or one above it`);
  }
  const project = load(configFile);
  const content = readFileSync(file, "utf8");
  const key = cacheKey(project, file, content);
  const dir = cacheDir(project.root);
  return {
    configFile,
    absoluteConfig: resolve(configFile),
    absoluteFile: resolve(file),
    rel: relativePath(project.root, file),
    entry: join(dir, `${key}.js`),
    dependencies: [...project.files],
  };
}

function cached(lookup: Lookup): CompiledSql {
  return { code: readFileSync(lookup.entry, "utf8"), dependencies: lookup.dependencies };
}

function requireBinary(lookup: Lookup): string {
  const bin = binary(lookup.absoluteConfig);
  if (bin !== null) return bin;
  const dir = dirname(lookup.entry);
  throw new Error(
    `typedpg: ${lookup.rel} has no entry in the typedpg cache (${dir}), and no \`typedpg\` binary is ` +
      `installed to compile it: run \`typedpg build\` (from @cubos/typedpg-cli) where the project ` +
      `is built, or install @cubos/typedpg-cli as a devDependency to compile on demand.`,
  );
}

/** The entry `build` filled, or the error its report gives this file. */
function settle(lookup: Lookup, report: Report): CompiledSql {
  if (existsSync(lookup.entry)) return cached(lookup);
  throw errorOf(lookup, report.diagnostics) ?? notCompiled(lookup);
}

/** The file's errors from a report, or `null` when it has none. */
function errorOf(lookup: Lookup, diagnostics: Diagnostic[]): Error | null {
  const errors = diagnostics.filter(
    (d) => d.file === lookup.rel || d.file === lookup.rel.split("/").join(sep),
  );
  if (errors.length === 0) return null;
  const lines = errors.map((d) =>
    d.line == null ? `${d.file}: ${d.message}` : `${d.file}:${d.line}:${d.col}: ${d.message}`,
  );
  return new Error(`typedpg: ${lookup.rel} has errors:\n${lines.join("\n")}`);
}

function notCompiled(lookup: Lookup): Error {
  return new Error(
    `typedpg: \`typedpg build\` did not compile ${lookup.rel}: is it one of the files \`sql.include\` names in ${lookup.configFile}?`,
  );
}

/** The nearest `typedpg.config.json` at or above `dir`. */
function findConfig(dir: string): string | null {
  const cached = configs.get(dir);
  if (cached !== undefined) return cached;
  const candidate = join(dir, CONFIG_FILE);
  const parent = dirname(dir);
  const found = existsSync(candidate) ? candidate : parent === dir ? null : findConfig(parent);
  configs.set(dir, found);
  return found;
}

interface Report {
  diagnostics: Diagnostic[];
}

/** Run `bin` (`typedpg build`), which fills the cache. */
function build(bin: string, configFile: string): Report {
  let stdout: string;
  try {
    stdout = execFileSync(bin, ["build", "--json", "--config", configFile], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "inherit"],
      maxBuffer: 64 * 1024 * 1024,
    });
  } catch (e) {
    // It fails when a file has an error, the report still on stdout.
    const out = (e as { stdout?: string }).stdout;
    if (typeof out !== "string" || out.trim() === "") throw e;
    stdout = out;
  }
  return JSON.parse(stdout) as Report;
}

// --- the persistent `typedpg serve` child --------------------------------

/** What `typedpg/compile` returns. */
interface CompileResult {
  /** The loader's module; `null` when the file is not one of `sql.include`. */
  code: string | null;
  dependencies: string[];
  diagnostics: Diagnostic[];
}

interface Pending {
  resolve(value: CompileResult): void;
  reject(reason: unknown): void;
}

interface Serve {
  binary: string;
  child: ChildProcess;
  input: Writable;
  output: Readable;
  buffer: Buffer;
  nextId: number;
  pending: Map<number, Pending>;
  closed: boolean;
}

/** The serve child went away with this request in flight: try a fresh one. */
class ServeExit extends Error {}

const serves = new Map<string, Serve>();
let hookedExit = false;

function idleTimeout(): number {
  const raw = process.env.TYPEDPG_SERVE_IDLE_TIMEOUT;
  if (raw !== undefined && raw !== "") {
    const seconds = Number(raw);
    if (Number.isFinite(seconds) && seconds > 0) return seconds;
  }
  return DEFAULT_IDLE_TIMEOUT;
}

/** The child for `bin`, spawned on first use and again whenever it is gone. */
function serveFor(bin: string): Serve {
  const known = serves.get(bin);
  if (known !== undefined && !known.closed) return known;
  const child = spawn(bin, ["serve", "--idle-timeout", String(idleTimeout())], {
    stdio: ["pipe", "pipe", "inherit"],
  });
  const serve: Serve = {
    binary: bin,
    child,
    input: child.stdin!,
    output: child.stdout!,
    buffer: Buffer.alloc(0),
    nextId: 1,
    pending: new Map(),
    closed: false,
  };
  serves.set(bin, serve);
  // The child exists for the next cache miss, not for the host's lifetime.
  // A request in flight re-refs these (see `request`): its answer is the
  // host's work; once it is answered, nothing here keeps the process alive.
  unref(serve);
  child.stdout!.on("data", (chunk: Buffer) => receive(serve, chunk));
  child.stdout!.on("error", (error) => {
    fail(serve, new ServeExit(`typedpg: reading from the serve child failed (${error.message})`));
  });
  // A write to a child that just exited fails with EPIPE, which its exit
  // event turns into the pending request's rejection; don't crash on it.
  serve.input.on("error", () => {});
  child.on("error", (error) => {
    fail(serve, new ServeExit(`typedpg: the serve child failed to run (${error.message})`));
  });
  child.on("exit", (code, signal) => {
    fail(serve, new ServeExit(`typedpg: the serve child exited (${signal ?? code})`));
  });
  if (!hookedExit) {
    hookedExit = true;
    process.once("exit", () => {
      for (const serve of serves.values()) serve.child.kill();
    });
  }
  return serve;
}

function unref(serve: Serve): void {
  handle(serve.child, "unref");
  handle(serve.input, "unref");
  handle(serve.output, "unref");
}

function ref(serve: Serve): void {
  handle(serve.child, "ref");
  handle(serve.input, "ref");
  handle(serve.output, "ref");
}

function handle(stream: unknown, method: "ref" | "unref"): void {
  const fn = (stream as { ref?: () => void; unref?: () => void } | null)?.[method];
  fn?.call(stream);
}

function fail(serve: Serve, error: Error): void {
  if (serve.closed) return;
  serve.closed = true;
  if (serves.get(serve.binary) === serve) serves.delete(serve.binary);
  for (const pending of serve.pending.values()) pending.reject(error);
  serve.pending.clear();
}

function request(serve: Serve, params: { config: string; file: string }): Promise<CompileResult> {
  if (serve.closed) return Promise.reject(new ServeExit("typedpg: the serve child is gone"));
  const id = serve.nextId++;
  const body = JSON.stringify({ jsonrpc: "2.0", id, method: "typedpg/compile", params });
  return new Promise((resolve, reject) => {
    if (serve.pending.size === 0) ref(serve);
    serve.pending.set(id, { resolve, reject });
    try {
      serve.input.write(`Content-Length: ${Buffer.byteLength(body)}\r\n\r\n${body}`);
    } catch (error) {
      serve.pending.delete(id);
      if (serve.pending.size === 0) unref(serve);
      reject(error);
    }
  });
}

/**
 * One `typedpg/compile`, retried once on a fresh child when the first one
 * dies under it (an idle timeout that raced, a crash). Answers with an RPC
 * error are answers: they go to the caller.
 */
async function serveCompile(bin: string, config: string, file: string): Promise<CompileResult> {
  try {
    return await request(serveFor(bin), { config, file });
  } catch (error) {
    if (!(error instanceof ServeExit)) throw error;
    return await request(serveFor(bin), { config, file });
  }
}

function receive(serve: Serve, chunk: Buffer): void {
  serve.buffer = Buffer.concat([serve.buffer, chunk]);
  for (;;) {
    const headerEnd = serve.buffer.indexOf("\r\n\r\n");
    if (headerEnd < 0) return;
    const header = serve.buffer.subarray(0, headerEnd).toString("ascii");
    const length = Number(/content-length:\s*(\d+)/i.exec(header)?.[1]);
    if (!Number.isFinite(length)) {
      fail(serve, new ServeExit("typedpg: the serve child sent a malformed message"));
      return;
    }
    const start = headerEnd + 4;
    if (serve.buffer.length < start + length) return;
    const body = serve.buffer.subarray(start, start + length).toString("utf8");
    serve.buffer = serve.buffer.subarray(start + length);
    let message: { id?: number; result?: CompileResult; error?: { message?: string } };
    try {
      message = JSON.parse(body) as typeof message;
    } catch {
      continue;
    }
    if (message.id === undefined) continue;
    const pending = serve.pending.get(message.id);
    if (pending === undefined) continue;
    serve.pending.delete(message.id);
    if (serve.pending.size === 0) unref(serve);
    if (message.error !== undefined) {
      pending.reject(new Error(`typedpg: ${message.error.message ?? "the serve child failed"}`));
    } else {
      pending.resolve(message.result as CompileResult);
    }
  }
}

function load(configFile: string): Project {
  const known = projects.get(configFile);
  const root = dirname(resolve(configFile));
  const files = [configFile, ...migrationFiles(root, readFileSync(configFile, "utf8")).map((m) => m.path)];
  const signature = files
    .map((f) => {
      const s = statSync(f);
      return `${f}\0${s.size}\0${s.mtimeMs}`;
    })
    .join("\n");
  if (known && known.signature === signature) return known;
  const project: Project = { configFile, root, identity: schemaIdentity(root, configFile), files, signature };
  projects.set(configFile, project);
  return project;
}

interface MigrationFile {
  name: string;
  path: string;
}

/** The databases of a config, sorted by name: `[name, migration directories]`. */
function databases(root: string, text: string): [string, string[]][] {
  type Db = { migrations?: string | { dir?: string }; extraMigrations?: string[] };
  const config = JSON.parse(text) as Db & { databases?: Record<string, Db> };
  const entries: [string, Db][] = config.databases
    ? Object.entries(config.databases).sort((a, b) => Buffer.compare(Buffer.from(a[0]), Buffer.from(b[0])))
    : [["default", config]];
  return entries.map(([name, db]) => {
    const dir = typeof db.migrations === "string" ? db.migrations : (db.migrations?.dir ?? "migrations");
    return [name, [dir, ...(db.extraMigrations ?? [])].map((d) => resolve(root, d))];
  });
}

/**
 * The migrations of `dirs` as the catalog applies them: `.sql` files but
 * `.down.sql` ones, by file name (bytes), a directory's before the next's on
 * a tie.
 */
function migrations(dirs: string[]): MigrationFile[] {
  const files: MigrationFile[] = [];
  for (const dir of dirs) {
    let names: string[];
    try {
      names = readdirSync(dir);
    } catch {
      continue;
    }
    for (const name of names) {
      if (name.endsWith(".sql") && !name.endsWith(".down.sql")) files.push({ name, path: join(dir, name) });
    }
  }
  return files.sort((a, b) => Buffer.compare(Buffer.from(a.name), Buffer.from(b.name)));
}

function migrationFiles(root: string, text: string): MigrationFile[] {
  return databases(root, text).flatMap(([, dirs]) => migrations(dirs));
}

/** A SHA-256 of the config file and every database's migrations. */
function schemaIdentity(root: string, configFile: string): string {
  const text = readFileSync(configFile);
  const h = createHash("sha256");
  h.update("typedpg-schema-1\n");
  h.update(text);
  h.update("\n");
  for (const [name, dirs] of databases(root, text.toString("utf8"))) {
    h.update(`db:${name}\n`);
    for (const m of migrations(dirs)) {
      h.update(m.name);
      h.update("\0");
      h.update(readFileSync(m.path));
      h.update("\0");
    }
  }
  return h.digest("hex");
}

/** `file` relative to `root`, with `/`, both without symlinks. */
function relativePath(root: string, file: string): string {
  return relative(realpathSync(root), realpathSync(file)).split(sep).join("/");
}

function cacheKey(project: Project, file: string, content: string): string {
  const h = createHash("sha256");
  for (const part of [FORMAT, VERSION, project.identity, relativePath(project.root, file), content]) {
    h.update(part);
    h.update("\n");
  }
  return h.digest("hex");
}

function cacheDir(root: string): string {
  return process.env.TYPEDPG_CACHE_DIR || join(root, "node_modules", ".cache", "typedpg");
}
