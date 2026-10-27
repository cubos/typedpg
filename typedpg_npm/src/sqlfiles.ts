// What the loaders and bundler plugins share: the ES module of a `.sql`
// file, read from the cache `typedpg build` fills.
//
// The cache is keyed by everything a module depends on — the typedpg
// version, the config file, every database's migrations, the file's path and
// content — computed here as the binary computes it (typedpg_ts/src/cache.rs:
// change the recipe in both). A file with no entry runs `typedpg build`
// once — when the install has a binary, which only a devDependency on
// `@cubos/typedpg-cli` gives it — and that compiles every file of the
// project missing one: importing a `.sql` file never analyzes it in this
// process, and a warm cache (`typedpg build` in the image's build) needs no
// binary at all.

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, readFileSync, readdirSync, realpathSync, statSync } from "node:fs";
import { dirname, join, relative, resolve, sep } from "node:path";
import { binary } from "./binary.js";

const FORMAT = "typedpg-cache-1";
const CONFIG_FILE = "typedpg.config.json";

const VERSION: string = (
  JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8")) as { version: string }
).version;

/** A compiled `.sql` file: its module, and the files it depends on (to watch). */
export interface CompiledSql {
  code: string;
  dependencies: string[];
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

const projects = new Map<string, Project>();
const configs = new Map<string, string | null>();

/** The module of the `.sql` file at `file` (an absolute path). */
export function compileSql(file: string): CompiledSql {
  const configFile = findConfig(dirname(file));
  if (configFile === null) {
    throw new Error(`typedpg: no ${CONFIG_FILE} in the directory of ${file} or one above it`);
  }
  const project = load(configFile);
  const content = readFileSync(file, "utf8");
  const key = cacheKey(project, file, content);
  const dir = cacheDir(project.root);
  const entry = join(dir, `${key}.js`);
  const dependencies = [...project.files];
  if (existsSync(entry)) return { code: readFileSync(entry, "utf8"), dependencies };

  const bin = binary(resolve(configFile));
  if (bin === null) {
    const rel = relativePath(project.root, file);
    throw new Error(
      `typedpg: ${rel} has no entry in the typedpg cache (${dir}), and no \`typedpg\` binary is ` +
        `installed to compile it: run \`typedpg build\` (from @cubos/typedpg-cli) where the project ` +
        `is built, or install @cubos/typedpg-cli as a devDependency to compile on demand.`,
    );
  }
  const report = build(bin, configFile);
  if (existsSync(entry)) return { code: readFileSync(entry, "utf8"), dependencies };
  const rel = relativePath(project.root, file);
  const errors = report.diagnostics.filter((d) => d.file === rel || d.file === rel.split("/").join(sep));
  if (errors.length > 0) {
    const lines = errors.map((d) => (d.line == null ? `${d.file}: ${d.message}` : `${d.file}:${d.line}:${d.col}: ${d.message}`));
    throw new Error(`typedpg: ${rel} has errors:\n${lines.join("\n")}`);
  }
  throw new Error(
    `typedpg: \`typedpg build\` did not compile ${rel}: is it one of the files \`sql.include\` names in ${configFile}?`,
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
  diagnostics: { file: string; line: number | null; col: number | null; message: string }[];
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
