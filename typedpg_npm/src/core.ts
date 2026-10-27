// The bridge to `typedpg_wasm`, the Rust migration helpers compiled to
// WebAssembly: one implementation of statement splitting, drift
// normalization and hashing, migration ordering and file-name parsing, so
// the npm runner and the Rust one cannot disagree. See typedpg_wasm/src/lib.rs
// for the ABI.
//
// The module is compiled lazily, on the first call that needs it — never
// when the package is imported. Query-only users (`createSql`, codecs,
// loaders) never pay for it, and a Cloudflare Worker, where compiling wasm
// from bytes is forbidden, only hits it if it runs migrations.

import { WASM_MODULE_BASE64 } from "./wasm.generated.js";

/** The module's exports; the shapes are `typedpg_wasm`'s ABI. */
interface Exports {
  readonly memory: WebAssembly.Memory;
  alloc(len: number): number;
  dealloc(ptr: number, len: number): void;
  output_ptr(): number;
  output_len(): number;
  split_statements(ptr: number, len: number): number;
  normalize(ptr: number, len: number): number;
  hash(ptr: number, len: number): number;
  hash_version(): number;
  drift_message(
    namePtr: number,
    nameLen: number,
    storedPtr: number,
    storedLen: number,
    hasStored: number,
    filePtr: number,
    fileLen: number,
    failOnDrift: number,
  ): number;
  locate_position(ptr: number, len: number, base: number, position: number): number;
  parse_migration_name(
    namePtr: number,
    nameLen: number,
    pathPtr: number,
    pathLen: number,
  ): number;
  compare_versions(aPtr: number, aLen: number, bPtr: number, bLen: number): number;
  compare_migration_names(aPtr: number, aLen: number, bPtr: number, bLen: number): number;
  validate_table_name(ptr: number, len: number): number;
  strip_bom(ptr: number, len: number): number;
  has_no_transaction_marker(ptr: number, len: number): number;
}

const encoder = new TextEncoder();
const decoder = new TextDecoder();

let instance: Exports | undefined;

/** The module, compiled on first use (and once). */
function wasm(): Exports {
  if (instance === undefined) {
    const module = new WebAssembly.Module(decodeBase64(WASM_MODULE_BASE64));
    instance = new WebAssembly.Instance(module, {}).exports as unknown as Exports;
  }
  return instance;
}

/**
 * `base64` as bytes, through `atob`: it is a global in Node, Deno, Bun and
 * workerd alike, so the same build runs everywhere (Node's `Buffer` is not).
 */
function decodeBase64(base64: string): Uint8Array<ArrayBuffer> {
  const binary = atob(base64);
  const buffer = new ArrayBuffer(binary.length);
  const bytes = new Uint8Array(buffer);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

/** An operation's status (0 is success) and its output buffer. */
interface Outcome {
  readonly status: number;
  readonly output: Uint8Array;
}

/**
 * Run an operation of the module: `f` gets the exports and a way to copy
 * inputs into the module's memory, which are freed afterwards. The output is
 * copied before the next call can overwrite it.
 */
function run(
  f: (exports: Exports, input: (value: string | Uint8Array) => [ptr: number, len: number]) => number,
): Outcome {
  const exports = wasm();
  const allocated: [ptr: number, len: number][] = [];
  const input = (value: string | Uint8Array): [ptr: number, len: number] => {
    const bytes = typeof value === "string" ? encoder.encode(value) : value;
    // The module rounds empty buffers up to one byte; dealloc gets the
    // allocated length, the operation the real one.
    const allocatedLen = Math.max(bytes.length, 1);
    const ptr = exports.alloc(allocatedLen);
    if (bytes.length > 0) new Uint8Array(exports.memory.buffer, ptr, bytes.length).set(bytes);
    allocated.push([ptr, allocatedLen]);
    return [ptr, bytes.length];
  };
  try {
    const status = f(exports, input);
    const output = new Uint8Array(
      exports.memory.buffer,
      exports.output_ptr(),
      exports.output_len(),
    ).slice();
    return { status, output };
  } finally {
    for (const [ptr, len] of allocated) exports.dealloc(ptr, len);
  }
}

/** The output's UTF-8 text; throws the module's message on failure. */
function text({ status, output }: Outcome): string {
  const value = decoder.decode(output);
  if (status !== 0) throw new Error(value || "typedpg: a wasm operation failed");
  return value;
}

/** The output's text, or `null` when the operation succeeded. */
function errorText({ status, output }: Outcome): string | null {
  return status === 0 ? null : decoder.decode(output) || "typedpg: a wasm operation failed";
}

/** The results are comparisons: `<`, `=` or `>`. */
function comparison(outcome: Outcome): number {
  const { output } = outcome;
  text(outcome);
  const byte = output[0];
  return byte === 0x3c ? -1 : byte === 0x3e ? 1 : 0;
}

/** The version of the normalization whose hashes the tracking table stores. */
export function hashVersion(): number {
  const outcome = run((exports) => exports.hash_version());
  text(outcome);
  return new DataView(outcome.output.buffer, outcome.output.byteOffset, outcome.output.byteLength).getInt32(0, true);
}

/** The stored drift hash of `sql`. */
export function hash(sql: string): string {
  return text(run((exports, input) => exports.hash(...input(sql))));
}

/** `sql` normalized so comments, whitespace and unquoted case are not drift. */
export function normalize(sql: string): string {
  return text(run((exports, input) => exports.normalize(...input(sql))));
}

/**
 * The drift message: `name`, a unified diff of `storedSql` (what was applied)
 * against `fileSql` (the file now), and how to record an intentional edit.
 */
export function driftMessage(
  name: string,
  storedSql: string | null,
  fileSql: string,
  failOnDrift: boolean,
): string {
  return text(
    run((exports, input) => {
      const [namePtr, nameLen] = input(name);
      const [storedPtr, storedLen] = storedSql === null ? [0, 0] : input(storedSql);
      const [filePtr, fileLen] = input(fileSql);
      return exports.drift_message(
        namePtr,
        nameLen,
        storedPtr,
        storedLen,
        storedSql === null ? 0 : 1,
        filePtr,
        fileLen,
        failOnDrift ? 1 : 0,
      );
    }),
  );
}

/**
 * PG's 1-based character `position` in `sql.slice(base)` as `LINE n:` and a
 * caret. `base` is an offset into the JS string (UTF-16 code units), as
 * {@link splitStatements} reports it; the Rust helper indexes UTF-8 bytes.
 */
export function locatePosition(sql: string, base: number, position: number): string {
  const baseBytes = encoder.encode(sql.slice(0, Math.max(base, 0))).length;
  return text(
    run((exports, input) => exports.locate_position(...input(sql), baseBytes, position)),
  );
}

/** A migration file stem's version, or the message explaining why it is malformed. */
export type ParsedMigrationName = { version: string } | { error: string };

/** Parse `NNNN_description`, as the Rust runner's `MigrationSource` does. */
export function parseMigrationName(name: string, path: string): ParsedMigrationName {
  const outcome = run((exports, input) => {
    const [namePtr, nameLen] = input(name);
    const [pathPtr, pathLen] = input(path);
    return exports.parse_migration_name(namePtr, nameLen, pathPtr, pathLen);
  });
  const message = errorText(outcome);
  return message === null ? { version: decoder.decode(outcome.output) } : { error: message };
}

/** Compare two migration versions as numbers: `2` before `10`. */
export function compareVersions(a: string, b: string): number {
  return comparison(
    run((exports, input) => exports.compare_versions(...input(a), ...input(b))),
  );
}

/** Compare two migration names by version, then by name. */
export function compareMigrationNames(a: string, b: string): number {
  return comparison(
    run((exports, input) => exports.compare_migration_names(...input(a), ...input(b))),
  );
}

/**
 * Why `table` is not a valid tracking-table name, or `null` when it is: the
 * Rust runner's `MigrationsConfig::validate` rule and message.
 */
export function validateTableName(table: string): string | null {
  return errorText(run((exports, input) => exports.validate_table_name(...input(table))));
}

/** `sql` without a leading byte-order mark. */
export function stripBom(sql: string): string {
  return text(run((exports, input) => exports.strip_bom(...input(sql))));
}

/** Whether `sql`'s first line is the `-- no-transaction` marker. */
export function hasNoTransactionMarker(sql: string): boolean {
  const outcome = run((exports, input) => exports.has_no_transaction_marker(...input(sql)));
  text(outcome);
  return outcome.output[0] === 1;
}

/**
 * The statements of `sql`, each with its character offset in the JS string
 * (a statement keeps its terminating `;`; the last may have none; pieces of
 * only whitespace and comments are dropped).
 *
 * The module reports byte offsets into the UTF-8 encoding — the whole text
 * is decoded once, so a multi-byte character anywhere does not shift the
 * offsets JS sees.
 */
export function splitStatements(sql: string): [number, string][] {
  const bytes = encoder.encode(sql);
  const outcome = run((exports, input) => exports.split_statements(...input(bytes)));
  text(outcome);
  const output = outcome.output;
  const view = new DataView(output.buffer, output.byteOffset, output.byteLength);
  const statements: [number, string][] = [];
  let charOffset = 0;
  let byteOffset = 0;
  for (let i = 0; i + 8 <= output.byteLength; i += 8) {
    const start = view.getUint32(i, true);
    const length = view.getUint32(i + 4, true);
    charOffset += decoder.decode(bytes.subarray(byteOffset, start)).length;
    byteOffset = start;
    statements.push([charOffset, decoder.decode(bytes.subarray(start, start + length))]);
  }
  return statements;
}
