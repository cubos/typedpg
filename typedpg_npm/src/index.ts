// The runtime of the modules `typedpg gen` generates. A generated module
// calls `createSql` with its `Queries` interface (each query's parameter and
// row types, keyed by the query's text) and the table of what each query
// runs, and exports the `sql` it returns:
//
//   import { sql } from "./db";
//   const user = await sql("SELECT name FROM users WHERE id = $id").fetchOne(pool, { id });

import { type Codec, decode, encode, setOwn } from "./codec.js";
import { type Executor, type RawResult, driverFor, unsupported } from "./adapters.js";

export { DecodeError, emptyRange, range } from "./codec.js";
export type { Codec, Interval, Range, RangeBound } from "./codec.js";
export type { Driver, Executor, PgQueryable, PostgresJsSql, RawResult } from "./adapters.js";
export {
  MigrationError,
  embedMigrations,
  loadMigrations,
  migrate,
  migrationStatus,
  revertMigration,
} from "./migrate.js";
export type { Migration, MigrationSet, MigrationStatus, MigrationsConfig } from "./migrate.js";

/** What a `json` / `jsonb` column decodes to. */
/**
 * `T`, which must fit `U`: the generated module checks with it that each
 * type `types` maps a PostgreSQL type to fits what the type is read as.
 */
export type Fits<T extends U, U> = T;

export type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };

/** A query's types, as the generated `Queries` interface describes it. */
export interface QueryTypes {
  params: object;
  row: object;
  /** The single column's type, for a query with one column. */
  value?: unknown;
}

/** The arguments after the executor: the parameters, optional without any. */
type Args<T extends QueryTypes> = {} extends T["params"] ? [params?: T["params"]] : [params: T["params"]];

export interface StreamOptions {
  /** How many rows are fetched from the server at a time (default 100). */
  batchSize?: number;
}

/** A typed query, ready to run on any executor. */
export interface Query<T extends QueryTypes> {
  readonly text: string;
  /** Type-only: what {@link Row} and {@link Params} read. */
  readonly "~types"?: T;
  /** Run the query and return all its rows. */
  fetchAll(executor: Executor, ...args: Args<T>): Promise<T["row"][]>;
  /** Run the query and return its one row; a {@link NoRowsError} or {@link TooManyRowsError} otherwise. */
  fetchOne(executor: Executor, ...args: Args<T>): Promise<T["row"]>;
  /** Run the query and return its row, or `null` without one; a {@link TooManyRowsError} with more. */
  fetchOptional(executor: Executor, ...args: Args<T>): Promise<T["row"] | null>;
  /**
   * Run the query and yield its rows as the server sends them, through a
   * cursor. Run on a pool, it holds one connection until the iteration
   * ends (or is broken out of). On node-postgres it needs `pg-cursor`.
   */
  fetchStream(executor: Executor, ...args: [...Args<T>, options?: StreamOptions]): AsyncIterable<T["row"]>;
  /** Run the statement and return the number of rows it affected. */
  execute(executor: Executor, ...args: Args<T>): Promise<number>;
}

/** A query with a single column, which can return its value directly. */
export interface ValueQuery<T extends QueryTypes> extends Query<T> {
  /** `fetchOne`'s single column. */
  fetchValue(executor: Executor, ...args: Args<T>): Promise<T["value"]>;
  /** `fetchOptional`'s single column; `null` without a row. */
  fetchValueOptional(executor: Executor, ...args: Args<T>): Promise<T["value"] | null>;
}

type QueryOf<T> = T extends QueryTypes ? ("value" extends keyof T ? ValueQuery<T> : Query<T>) : never;

/**
 * The text itself when the generated module has it without an error;
 * otherwise a message, which TypeScript shows as the type the text is not
 * assignable to.
 *
 * `Q extends { [P in K]: infer D }` is a property lookup: `K extends keyof Q`
 * says the same, but TypeScript 5 relates `K` to the union of every query's
 * text, which made checking quadratic in the number of queries (8000
 * queries: 51 s of CPU against 4 s).
 */
type Checked<Q, K extends string, Missing extends string> = Q extends { [P in K]: infer D }
  ? D extends { error: infer E extends string }
    ? `typedpg: ${E}`
    : K
  : Missing;

/**
 * What a query with an error returns: anything goes, so that the error at
 * its text is the only one reported.
 */
type ErrorQuery = ValueQuery<{ params: any; row: any; value: any }>;

/** The `sql` a generated module exports. */
export type Sql<Q> = <K extends string>(
  text: Checked<Q, K, "typedpg: this query is not in the generated module: run `typedpg gen`">,
) => Q extends { [P in K]: infer D } ? (D extends QueryTypes ? QueryOf<D> : ErrorQuery) : ErrorQuery;

/** A row type: `Row<typeof query>`. */
export type Row<Q> = Q extends { "~types"?: infer T extends QueryTypes } ? T["row"] : never;
/** A parameters type: `Params<typeof query>`. */
export type Params<Q> = Q extends { "~types"?: infer T extends QueryTypes } ? T["params"] : never;

/** What the generator writes for each query: the SQL to run and the codecs. */
export interface QuerySpec {
  /** The SQL, rewritten to `$1`, `$2`, …; split where each spread goes. */
  sql: readonly string[];
  /** The parameters, in `$1`, `$2`, … order. */
  params: readonly (readonly [string, Codec])[];
  /**
   * Each spread's name, fields (name, codec, the cast after its
   * placeholder) and kind: `rows` of VALUES (`VALUES $..rows { a, b }`), or
   * the `list` of an IN (`x IN $..ids`), whose one field is the element.
   */
  spreads?: readonly (readonly [string, readonly (readonly [string, Codec, string])[], "rows" | "list"])[];
  columns: readonly (readonly [string, Codec])[];
  /** Whether `fetchOne` / `fetchOptional` can wrap the SQL in `LIMIT 2`. */
  subquery?: boolean;
}

export class TypedpgError extends Error {
  override name = "TypedpgError";
}

/** `fetchOne` / `fetchValue` found no row. */
export class NoRowsError extends TypedpgError {
  override name = "NoRowsError";
  constructor(readonly query: string) {
    super(`query returned no rows: \`${query}\``);
  }
}

/** `fetchOne` / `fetchOptional` / `fetchValue` found more than one row. */
export class TooManyRowsError extends TypedpgError {
  override name = "TooManyRowsError";
  constructor(readonly query: string) {
    super(`query returned more than one row: \`${query}\``);
  }
}

/**
 * A column name as the generated module has it: without its nullability
 * annotation (`AS "title!"` is the column `title` on the wire too), as the
 * analyzer strips it — but `?column?`, PG's own name, is no annotation.
 */
function withoutAnnotation(name: string): string {
  return name === "?column?" ? name : name.replace(/[!?]$/, "");
}

/** A table of JSON specs, each parsed the first time it is used. */
function specTable<S>(specs: Record<string, string>, what: string): (text: string) => S {
  const parsed = new Map<string, S>();
  return (text) => {
    let spec = parsed.get(text);
    if (spec === undefined) {
      if (!Object.hasOwn(specs, text)) {
        throw new TypedpgError(`${what} not in the generated module, run \`typedpg gen\`: \`${text}\``);
      }
      spec = JSON.parse(specs[text]!) as S;
      parsed.set(text, spec);
    }
    return spec;
  };
}

/**
 * Called by generated modules, with each query's {@link QuerySpec} as JSON:
 * parsed the first time the query is used, so loading a module with
 * thousands of queries parses none of them.
 */
export function createSql<Q>(specs: Record<string, string>): Sql<Q> {
  const spec = specTable<QuerySpec>(specs, "query");
  return ((text: string) => new QueryImpl(text, spec(text))) as unknown as Sql<Q>;
}

type Built = { text: string; values: (string | null)[]; empty: boolean };

class QueryImpl {
  constructor(
    readonly text: string,
    private readonly spec: QuerySpec,
  ) {}

  private build(params: Record<string, unknown> = {}): Built {
    const { spec } = this;
    const values = spec.params.map(([name, codec]) => encode(codec, params[name]));
    let text = spec.sql[0]!;
    let empty = false;
    let n = values.length;
    spec.spreads?.forEach(([name, fields, kind], i) => {
      if (kind === "list") {
        // `x IN` an empty list is false, `x NOT IN` it true: PG has no
        // syntax for one, so it is a subquery returning no row.
        const [, codec, cast] = fields[0]!;
        const items = params[name] as readonly unknown[];
        text +=
          items.length === 0
            ? `(SELECT NULL${cast} WHERE false)`
            : "(" +
              items
                .map((item) => {
                  values.push(encode(codec, item));
                  return `$${++n}${cast}`;
                })
                .join(", ") +
              ")";
        text += spec.sql[i + 1]!;
        return;
      }
      // No row is no VALUES row: the query isn't run.
      const rows = params[name] as readonly Record<string, unknown>[];
      if (rows.length === 0) empty = true;
      text += rows
        .map(
          (row) =>
            "(" +
            fields
              .map(([field, codec, cast]) => {
                values.push(encode(codec, row[field]));
                return `$${++n}${cast}`;
              })
              .join(", ") +
            ")",
        )
        .join(", ");
      text += spec.sql[i + 1]!;
    });
    return { text, values, empty };
  }

  /** Fail if `fields` (when the driver reports them) aren't the generated columns. */
  private checkFields(fields: string[] | undefined) {
    const { columns } = this.spec;
    // A schema change the generated module hasn't caught up with (`SELECT *`
    // after a migration added a column) would decode the wrong columns.
    if (fields && fields.map(withoutAnnotation).join("\0") !== columns.map((c) => c[0]).join("\0")) {
      throw new TypedpgError(
        `the query's columns (${fields.join(", ")}) are not the ones it was generated with ` +
          `(${columns.map((c) => c[0]).join(", ")}): run \`typedpg gen\``,
      );
    }
  }

  private decodeRow(raw: (string | null)[]): Record<string, unknown> {
    const { columns } = this.spec;
    const row: Record<string, unknown> = {};
    for (let i = 0; i < columns.length; i++) {
      const [name, codec] = columns[i]!;
      const v = raw[i];
      setOwn(row, name, v === null || v === undefined ? null : decode(codec, v));
    }
    return row;
  }

  private decodeRows(result: RawResult): Record<string, unknown>[] {
    this.checkFields(result.fields);
    return result.rows.map((raw) => this.decodeRow(raw));
  }

  async fetchAll(executor: Executor, params?: Record<string, unknown>) {
    const q = this.build(params);
    if (q.empty) return [];
    return this.decodeRows(await driverFor(executor).typedpgQuery(q.text, q.values));
  }

  async fetchOptional(executor: Executor, params?: Record<string, unknown>) {
    const q = this.build(params);
    if (q.empty) return null;
    const text = this.spec.subquery ? `SELECT * FROM (${q.text}) AS __typedpg_limit LIMIT 2` : q.text;
    const rows = this.decodeRows(await driverFor(executor).typedpgQuery(text, q.values));
    if (rows.length > 1) throw new TooManyRowsError(this.text);
    return rows[0] ?? null;
  }

  async fetchOne(executor: Executor, params?: Record<string, unknown>) {
    const row = await this.fetchOptional(executor, params);
    if (row === null) throw new NoRowsError(this.text);
    return row;
  }

  async *fetchStream(executor: Executor, params?: Record<string, unknown>, options?: StreamOptions) {
    const q = this.build(params);
    if (q.empty) return;
    const driver = driverFor(executor);
    if (!driver.typedpgStream) throw unsupported("typedpgStream (fetchStream)");
    for await (const rows of driver.typedpgStream(q.text, q.values, options?.batchSize ?? 100)) {
      for (const raw of rows) yield this.decodeRow(raw);
    }
  }

  async execute(executor: Executor, params?: Record<string, unknown>) {
    const q = this.build(params);
    if (q.empty) return 0;
    return (await driverFor(executor).typedpgQuery(q.text, q.values)).rowCount;
  }

  async fetchValue(executor: Executor, params?: Record<string, unknown>) {
    const row = await this.fetchOne(executor, params);
    return row[this.spec.columns[0]![0]];
  }

  async fetchValueOptional(executor: Executor, params?: Record<string, unknown>) {
    const row = await this.fetchOptional(executor, params);
    return row === null ? null : row[this.spec.columns[0]![0]];
  }
}

// ── COPY ──

/** A COPY target's types, as the generated `CopyTargets` interface describes it. */
export interface CopyTypes {
  row: object;
}

/** A typed `COPY ... FROM STDIN`, ready to run on any executor. */
export interface CopyInQuery<T extends CopyTypes> {
  readonly target: string;
  /**
   * Stream `rows` into the table and return how many were copied. The rows
   * are encoded one at a time as the COPY consumes them; if any fails, the
   * whole COPY is aborted and no row is kept. On node-postgres it needs
   * `pg-copy-streams`.
   */
  execute(executor: Executor, rows: Iterable<T["row"]> | AsyncIterable<T["row"]>): Promise<number>;
}

type CopyErrorQuery = CopyInQuery<{ row: any }>;

/** A COPY target's row type: `CopyRow<typeof target>`. */
export type CopyRow<C> = C extends CopyInQuery<infer T> ? T["row"] : never;

/** The `copyIn` a generated module exports. */
export type CopyIn<C> = <K extends string>(
  target: Checked<C, K, "typedpg: this COPY target is not in the generated module: run `typedpg gen`">,
) => C extends { [P in K]: infer D } ? (D extends CopyTypes ? CopyInQuery<D> : CopyErrorQuery) : CopyErrorQuery;

/** What the generator writes for each COPY target. */
export interface CopySpec {
  /** `COPY table (columns) FROM STDIN`. */
  sql: string;
  /** Each row's values, in order. */
  columns: readonly (readonly [string, Codec])[];
}

/** Called by generated modules, as {@link createSql}. */
export function createCopyIn<C>(specs: Record<string, string>): CopyIn<C> {
  const spec = specTable<CopySpec>(specs, "COPY target");
  return ((target: string) => {
    const s = spec(target);
    return {
      target,
      async execute(executor: Executor, rows: Iterable<Record<string, unknown>> | AsyncIterable<Record<string, unknown>>) {
        const driver = driverFor(executor);
        if (!driver.typedpgCopyIn) throw unsupported("typedpgCopyIn (copyIn)");
        return driver.typedpgCopyIn(s.sql, copyText(s, rows));
      },
    };
  }) as unknown as CopyIn<C>;
}

/** How much COPY text is sent at a time. */
const COPY_CHUNK = 64 * 1024;

/** `rows` in COPY's text format, a chunk at a time. */
async function* copyText(
  spec: CopySpec,
  rows: Iterable<Record<string, unknown>> | AsyncIterable<Record<string, unknown>>,
): AsyncIterable<string> {
  let chunk = "";
  for await (const row of rows) {
    for (let i = 0; i < spec.columns.length; i++) {
      const [name, codec] = spec.columns[i]!;
      const text = encode(codec, row[name]);
      if (i > 0) chunk += "\t";
      chunk += text === null ? "\\N" : copyEscape(text);
    }
    chunk += "\n";
    if (chunk.length >= COPY_CHUNK) {
      yield chunk;
      chunk = "";
    }
  }
  if (chunk.length > 0) yield chunk;
}

/** A value in COPY's text format: backslash, and the delimiters it uses. */
function copyEscape(text: string): string {
  return text.replace(/[\\\n\r\t]/g, (c) => (c === "\\" ? "\\\\" : c === "\n" ? "\\n" : c === "\r" ? "\\r" : "\\t"));
}
