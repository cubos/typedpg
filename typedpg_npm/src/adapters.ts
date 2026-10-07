// The drivers a query runs on. The application opens its connections as it
// already does and hands the client, pool or transaction to the query; the
// adapter runs it with every value in PG's text format, untouched by the
// driver's own type parsing, which the codecs then decode.

/** What an adapter returns: the raw text of each value, by column position. */
export interface RawResult {
  rows: (string | null)[][];
  /** The column names, when the driver reports them. */
  fields?: string[];
  /** Rows affected (INSERT / UPDATE / DELETE / …) or returned. */
  rowCount: number;
}

/**
 * Runs queries for typedpg on a driver it doesn't know. Only `typedpgQuery`
 * is needed for queries; the others enable `fetchStream`, `copyIn` and the
 * migration runner.
 */
export interface Driver {
  typedpgQuery(text: string, params: (string | null)[]): Promise<RawResult>;
  /** The rows of a query, a batch at a time. */
  typedpgStream?(text: string, params: (string | null)[], batchSize: number): AsyncIterable<(string | null)[][]>;
  /** Run `COPY ... FROM STDIN` (text format) with `data`; the rows copied. */
  typedpgCopyIn?(sql: string, data: AsyncIterable<string>): Promise<number>;
  /** Run `sql` (several statements) with the simple query protocol. */
  typedpgSimple?(sql: string): Promise<void>;
  /** A session pinned to one connection, and how to give it back. */
  typedpgSession?(): Promise<{ driver: Driver; release(): Promise<void> }>;
}

/** A node-postgres `Client`, `Pool` or `PoolClient`. */
export interface PgQueryable {
  query: (...args: any[]) => any;
}

/** A postgres.js `sql` (or a transaction's / reserved connection's). */
export interface PostgresJsSql {
  unsafe: (...args: any[]) => any;
  typed: (...args: any[]) => any;
}

/** Anything a query runs on. */
export type Executor = Driver | PgQueryable | PostgresJsSql;

/** node-postgres: every type parsed as the text it is. */
const rawTypes = { getTypeParser: () => (value: string) => value };

/** The driver for `executor`. */
export function driverFor(executor: Executor): Driver {
  if ("typedpgQuery" in executor) return executor;
  if ("unsafe" in executor) return postgresJs(executor);
  if ("query" in executor) return nodePostgres(executor);
  throw new TypeError("typedpg: not a node-postgres client or pool, a postgres.js sql, or a typedpg Driver");
}

/** An optional peer dependency, loaded when a feature needs it. */
async function load(name: string, feature: string): Promise<any> {
  try {
    const mod = await import(name);
    return mod.default ?? mod;
  } catch (e) {
    throw new Error(`typedpg: ${feature} on node-postgres needs the "${name}" package: npm install ${name}`, {
      cause: e,
    });
  }
}

function nodePostgres(pg: PgQueryable): Driver {
  const any = pg as any;
  // A Pool: a dedicated client for what spans several messages.
  const isPool = typeof any.totalCount === "number";
  const withClient = async <T>(f: (client: any) => Promise<T>): Promise<T> => {
    if (!isPool) return f(pg);
    const client = await any.connect();
    try {
      return await f(client);
    } finally {
      client.release();
    }
  };
  return {
    async typedpgQuery(text, params) {
      const result = await any.query({ text, values: params, rowMode: "array", types: rawTypes });
      return {
        rows: result.rows,
        fields: result.fields?.map((f: { name: string }) => f.name),
        rowCount: result.rowCount ?? result.rows.length,
      };
    },
    async *typedpgStream(text, params, batchSize) {
      const Cursor = await load("pg-cursor", "fetchStream");
      const client = isPool ? await any.connect() : pg;
      try {
        const cursor = client.query(new Cursor(text, params, { rowMode: "array", types: rawTypes }));
        try {
          for (;;) {
            const rows = await cursor.read(batchSize);
            if (rows.length === 0) return;
            yield rows;
          }
        } finally {
          await cursor.close();
        }
      } finally {
        if (isPool) client.release();
      }
    },
    async typedpgCopyIn(sql, data) {
      const { from } = await load("pg-copy-streams", "copyIn");
      return withClient(async (client) => {
        const stream = client.query(from(sql));
        const done = new Promise<void>((resolve, reject) => {
          stream.on("finish", resolve);
          stream.on("error", reject);
        });
        try {
          for await (const chunk of data) {
            if (!stream.write(chunk)) {
              await new Promise((resolve, reject) => {
                stream.once("drain", resolve);
                stream.once("error", reject);
              });
            }
          }
        } catch (e) {
          // Abort the COPY: no row of it is kept.
          stream.destroy(e);
          await done.catch(() => {});
          throw e;
        }
        stream.end();
        await done;
        return stream.rowCount as number;
      });
    },
    async typedpgSimple(sql) {
      await any.query(sql);
    },
    async typedpgSession() {
      if (!isPool) return { driver: nodePostgres(pg), release: async () => {} };
      const client = await any.connect();
      return {
        driver: nodePostgres(client),
        release: async () => client.release(),
      };
    },
  };
}

function postgresJs(sql: PostgresJsSql): Driver {
  const any = sql as any;
  // postgres.js serializes a parameter by the type the server infers for it
  // (`JSON.stringify` for a jsonb one), which would encode our text a second
  // time: declare each as `text` (oid 25), whose serializer leaves it as is
  // — the placeholder's cast (`($1::pg_catalog.jsonb)`) reads it as its type.
  const values = (params: (string | null)[]) => params.map((p) => (p === null ? null : any.typed(p, 25)));
  const text = (v: Uint8Array | null) => (v === null ? null : Buffer.from(v).toString("utf8"));
  return {
    async typedpgQuery(query, params) {
      // `.raw()`: each value's bytes, undecoded.
      const result = await any.unsafe(query, values(params)).raw();
      return {
        rows: result.map((row: (Uint8Array | null)[]) => row.map(text)),
        fields: result.columns?.map((c: { name: string }) => c.name),
        rowCount: result.count ?? result.length,
      };
    },
    async *typedpgStream(query, params, batchSize) {
      for await (const rows of any.unsafe(query, values(params)).raw().cursor(batchSize)) {
        yield rows.map((row: (Uint8Array | null)[]) => row.map(text));
      }
    },
    async typedpgCopyIn(copySql, data) {
      const reserved = typeof any.reserve === "function" ? await any.reserve() : null;
      const conn = reserved ?? any;
      try {
        const query = conn.unsafe(copySql);
        // postgres.js (3.4.9) never ends the stream when the server rejects
        // the COPY's data: the error only reaches the query's `reject`,
        // ignored once the query resolved to the stream. Hook it — without
        // it, a rejected COPY would wait forever.
        if (typeof query.reject !== "function") {
          throw new Error("typedpg: copyIn doesn't support this postgres.js version (no Query.reject to hook)");
        }
        const failed = new Promise<never>((_, reject) => {
          const original = query.reject;
          query.reject = (e: unknown) => {
            reject(e);
            return original(e);
          };
        });
        failed.catch(() => {});
        const stream = await query.writable();
        const finished = new Promise<void>((resolve, reject) => {
          stream.on("finish", resolve);
          stream.on("error", reject);
        });
        finished.catch(() => {});
        let rows = 0;
        try {
          for await (const chunk of data) {
            rows += countRows(chunk);
            if (!stream.write(chunk)) {
              await new Promise((resolve, reject) => {
                stream.once("drain", resolve);
                stream.once("error", reject);
              });
            }
          }
        } catch (e) {
          // Abort the COPY (CopyFail): no row of it is kept.
          stream.destroy(e);
          await Promise.race([finished, failed]).catch(() => {});
          throw e;
        }
        stream.end();
        await Promise.race([finished, failed]);
        // postgres.js doesn't report the server's count; a COPY keeps
        // every row or none, so it is the rows sent.
        return rows;
      } finally {
        await reserved?.release();
      }
    },
    async typedpgSimple(query) {
      await any.unsafe(query).simple();
    },
    async typedpgSession() {
      if (typeof any.reserve !== "function") {
        // Already one connection: a transaction's or a reserved `sql`.
        return { driver: postgresJs(sql), release: async () => {} };
      }
      const reserved = await any.reserve();
      return {
        driver: postgresJs(reserved),
        release: async () => reserved.release(),
      };
    },
  };
}

/** COPY text rows end with `\n` (a value's own newline is escaped). */
function countRows(chunk: string): number {
  let n = 0;
  for (let i = chunk.indexOf("\n"); i !== -1; i = chunk.indexOf("\n", i + 1)) n++;
  return n;
}

/** What `driver` lacks for `feature`. */
export function unsupported(feature: string): Error {
  return new TypeError(`typedpg: this Driver does not implement ${feature}`);
}
