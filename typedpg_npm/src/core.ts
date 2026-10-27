// The bridge to `typedpg_wasm`, the Rust migration runner compiled to
// WebAssembly: the sans-I/O migration machine and the file-name, ordering
// and tracking-table helpers around it, so the npm runner and the Rust one
// cannot disagree. Statement splitting, drift normalization and hashing, and
// the error and drift messages live inside the machine now. See
// typedpg_wasm/src/lib.rs for the ABI.
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
  migrator_new(ptr: number, len: number): number;
  migrator_step(handle: number, ptr: number, len: number): number;
  migrator_free(handle: number): void;
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

// The migration machine: the whole runner as a sans-I/O state machine in
// `typedpg_core::migrations::machine`, driven step by step. The format below
// mirrors `typedpg_wasm/src/codec.rs`: a tag byte, little-endian `u32`
// lengths and a null flag for an optional string.

/** What a run does. */
export type MachineAction =
  | { readonly kind: "up" }
  | { readonly kind: "status" }
  | { readonly kind: "revert"; readonly name: string | null; readonly force: boolean }
  | { readonly kind: "accept"; readonly name: string };

/** The runner settings the machine needs. */
export interface MachineConfig {
  readonly table: string;
  readonly lockId: number | bigint;
  readonly useTransaction: boolean;
  readonly failOnDrift: boolean;
}

/** One migration, as `loadMigrations`/`embedMigrations` produced it. */
export interface MachineMigration {
  readonly name: string;
  readonly sql: string;
  readonly downSql: string | null;
  readonly noTransaction: boolean;
  readonly downNoTransaction: boolean;
}

/** A database error, as the machine formats it. */
export interface MachineDbError {
  readonly code: string | null;
  readonly message: string;
  readonly severity: string | null;
  readonly detail: string | null;
  readonly hint: string | null;
  /** PG's 1-based character index into the text that was sent. */
  readonly position: number | null;
  readonly internalPosition: number | null;
  readonly internalQuery: string | null;
}

/** The driver's answer to the step it was given. */
export type MachineReply =
  | { readonly kind: "start" }
  | { readonly kind: "rows"; readonly rows: (string | null)[][] }
  | { readonly kind: "done" }
  | { readonly kind: "dbError"; readonly error: MachineDbError };

/** A status row; `appliedAt` is UTC RFC 3339 text. */
export interface MachineStatusRow {
  readonly name: string;
  readonly applied: boolean;
  readonly appliedAt: string | null;
  readonly drifted: boolean;
}

/** A finished action's result. */
export type MachineOutcome =
  | { readonly kind: "applied"; readonly names: string[] }
  | { readonly kind: "status"; readonly rows: MachineStatusRow[] }
  | { readonly kind: "reverted"; readonly name: string }
  | { readonly kind: "accepted" };

/** Why an action failed. */
export type MachineFailure = { readonly kind: "message"; readonly message: string } | { readonly kind: "db" };

/** What the machine asks the driver to do. */
export type MachineStep =
  | { readonly kind: "query"; readonly sql: string; readonly params: (string | null)[] }
  | { readonly kind: "execute"; readonly sql: string }
  | { readonly kind: "sleep"; readonly ms: number }
  | { readonly kind: "warn"; readonly message: string }
  | { readonly kind: "finish"; readonly outcome: MachineOutcome }
  | { readonly kind: "fail"; readonly failure: MachineFailure };

/**
 * Create a machine for `action`. Several may be alive at once: each run
 * awaits its driver between steps, and they must not share state.
 */
export function migratorNew(
  action: MachineAction,
  config: MachineConfig,
  migrations: readonly MachineMigration[],
): number {
  const input = encodeMachineInput(action, config, migrations);
  const outcome = run((exports, write) => exports.migrator_new(...write(input)));
  // The handle is the operation's return value; 0 is the failure status.
  if (outcome.status === 0) {
    throw new Error(
      decoder.decode(outcome.output) || "typedpg: could not create the migration machine",
    );
  }
  return outcome.status;
}

/** Advance the machine with its driver's `reply`; returns its next step. */
export function migratorStep(handle: number, reply: MachineReply): MachineStep {
  const input = encodeReply(reply);
  const outcome = run((exports, write) => exports.migrator_step(handle, ...write(input)));
  if (outcome.status !== 0) {
    throw new Error(
      decoder.decode(outcome.output) || "typedpg: the migration machine step failed",
    );
  }
  return decodeStep(new Reader(outcome.output));
}

/** Drop a machine; calling it twice is harmless. */
export function migratorFree(handle: number): void {
  wasm().migrator_free(handle);
}

function encodeMachineInput(
  action: MachineAction,
  config: MachineConfig,
  migrations: readonly MachineMigration[],
): Uint8Array {
  const writer = new Writer();
  switch (action.kind) {
    case "up":
      writer.u8(0);
      break;
    case "status":
      writer.u8(1);
      break;
    case "revert":
      writer.u8(2);
      writer.optionalString(action.name);
      writer.u8(action.force ? 1 : 0);
      break;
    case "accept":
      writer.u8(3);
      writer.string(action.name);
      break;
  }
  writer.string(config.table);
  writer.i64(BigInt(config.lockId));
  writer.u8(config.useTransaction ? 1 : 0);
  writer.u8(config.failOnDrift ? 1 : 0);
  writer.u32(migrations.length);
  for (const migration of migrations) {
    writer.string(migration.name);
    writer.string(migration.sql);
    writer.optionalString(migration.downSql);
    writer.u8(migration.noTransaction ? 1 : 0);
    writer.u8(migration.downNoTransaction ? 1 : 0);
  }
  return writer.finish();
}

function encodeReply(reply: MachineReply): Uint8Array {
  const writer = new Writer();
  switch (reply.kind) {
    case "start":
      writer.u8(0);
      break;
    case "rows":
      writer.u8(1);
      writer.u32(reply.rows.length);
      for (const row of reply.rows) {
        writer.u32(row.length);
        for (const value of row) writer.optionalString(value);
      }
      break;
    case "done":
      writer.u8(2);
      break;
    case "dbError": {
      writer.u8(3);
      const error = reply.error;
      writer.optionalString(error.code);
      writer.string(error.message);
      writer.optionalString(error.severity);
      writer.optionalString(error.detail);
      writer.optionalString(error.hint);
      writer.optionalU32(error.position);
      writer.optionalU32(error.internalPosition);
      writer.optionalString(error.internalQuery);
      break;
    }
  }
  return writer.finish();
}

function decodeStep(reader: Reader): MachineStep {
  const tag = reader.u8();
  switch (tag) {
    case 0: {
      const sql = reader.string();
      const count = reader.u32();
      const params: (string | null)[] = [];
      for (let i = 0; i < count; i++) params.push(reader.optionalString());
      return { kind: "query", sql, params };
    }
    case 1:
      return { kind: "execute", sql: reader.string() };
    case 2:
      return { kind: "sleep", ms: Number(reader.u64()) };
    case 3:
      return { kind: "warn", message: reader.string() };
    case 4:
      return { kind: "finish", outcome: decodeOutcome(reader) };
    case 5: {
      const failure = reader.u8();
      if (failure === 0) return { kind: "fail", failure: { kind: "message", message: reader.string() } };
      if (failure === 1) return { kind: "fail", failure: { kind: "db" } };
      throw new Error(`typedpg: unknown migration failure tag ${failure}`);
    }
    default:
      throw new Error(`typedpg: unknown migration step tag ${tag}`);
  }
}

function decodeOutcome(reader: Reader): MachineOutcome {
  const tag = reader.u8();
  switch (tag) {
    case 0: {
      const count = reader.u32();
      const names: string[] = [];
      for (let i = 0; i < count; i++) names.push(reader.string());
      return { kind: "applied", names };
    }
    case 1: {
      const count = reader.u32();
      const rows: MachineStatusRow[] = [];
      for (let i = 0; i < count; i++) {
        rows.push({
          name: reader.string(),
          applied: reader.boolean(),
          appliedAt: reader.optionalString(),
          drifted: reader.boolean(),
        });
      }
      return { kind: "status", rows };
    }
    case 2:
      return { kind: "reverted", name: reader.string() };
    case 3:
      return { kind: "accepted" };
    default:
      throw new Error(`typedpg: unknown migration outcome tag ${tag}`);
  }
}

/** A little-endian writer for the machine's binary format. */
class Writer {
  private readonly chunks: Uint8Array[] = [];
  private length = 0;

  u8(value: number): void {
    this.chunks.push(new Uint8Array([value]));
    this.length += 1;
  }

  u32(value: number): void {
    const bytes = new Uint8Array(4);
    new DataView(bytes.buffer).setUint32(0, value >>> 0, true);
    this.chunks.push(bytes);
    this.length += 4;
  }

  u64(value: bigint): void {
    const bytes = new Uint8Array(8);
    new DataView(bytes.buffer).setBigUint64(0, value, true);
    this.chunks.push(bytes);
    this.length += 8;
  }

  i64(value: bigint): void {
    const bytes = new Uint8Array(8);
    new DataView(bytes.buffer).setBigInt64(0, BigInt.asIntN(64, value), true);
    this.chunks.push(bytes);
    this.length += 8;
  }

  string(value: string): void {
    const bytes = encoder.encode(value);
    this.u32(bytes.length);
    this.chunks.push(bytes);
    this.length += bytes.length;
  }

  optionalString(value: string | null): void {
    if (value === null) {
      this.u8(0);
    } else {
      this.u8(1);
      this.string(value);
    }
  }

  optionalU32(value: number | null): void {
    if (value === null) {
      this.u8(0);
    } else {
      this.u8(1);
      this.u32(value);
    }
  }

  finish(): Uint8Array {
    const out = new Uint8Array(this.length);
    let at = 0;
    for (const chunk of this.chunks) {
      out.set(chunk, at);
      at += chunk.length;
    }
    return out;
  }
}

/** A bounds-checked reader for the machine's binary format. */
class Reader {
  private readonly view: DataView;
  private at = 0;

  constructor(private readonly bytes: Uint8Array) {
    this.view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  }

  private need(length: number): void {
    if (this.at + length > this.bytes.length) {
      throw new Error("typedpg: truncated migration machine output");
    }
  }

  u8(): number {
    this.need(1);
    return this.view.getUint8(this.at++);
  }

  boolean(): boolean {
    const value = this.u8();
    if (value > 1) throw new Error(`typedpg: invalid migration machine boolean ${value}`);
    return value === 1;
  }

  u32(): number {
    this.need(4);
    const value = this.view.getUint32(this.at, true);
    this.at += 4;
    return value;
  }

  u64(): bigint {
    this.need(8);
    const value = this.view.getBigUint64(this.at, true);
    this.at += 8;
    return value;
  }

  string(): string {
    const length = this.u32();
    this.need(length);
    const value = decoder.decode(this.bytes.subarray(this.at, this.at + length));
    this.at += length;
    return value;
  }

  optionalString(): string | null {
    const tag = this.u8();
    if (tag === 0) return null;
    if (tag !== 1) throw new Error(`typedpg: invalid migration machine option tag ${tag}`);
    return this.string();
  }
}
