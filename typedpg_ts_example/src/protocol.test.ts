// How each driver sends a query, read off the wire by a proxy (wire.ts):
// as the Rust runtime does, on the unnamed statement — never prepared and
// reused, so PostgreSQL plans each execution with its values instead of
// switching to a generic plan after five — with every parameter's type left
// for the server to infer from the cast `sql` writes after its placeholder,
// so a value is read as that type directly instead of converted from text.
//
// node-postgres sends a query in one round trip. postgres.js takes two when
// it has parameters: it describes a statement it hasn't prepared before
// binding it, and nothing in its API skips that but preparing it (see
// adapters.ts).

import assert from "node:assert/strict";
import { after, before, describe, test } from "node:test";

import pg from "pg";
import postgres from "postgres";
import type { Executor } from "@cubos/typedpg";

import * as q from "./queries.ts";
import { baseUrl, freshDatabase } from "./testdb.ts";
import { type Sent, Wire } from "./wire.ts";

/** The `unknown` pseudo-type: postgres.js's parameters are declared it, which the server resolves as unspecified. */
const UNKNOWN = 705;
const INT4 = 23;
const INT8 = 20;
const INT4_ARRAY = 1007;
const JSONB = 3802;
const TIMESTAMPTZ = 1184;

let url: string;
let wire: Wire;
let mood: number;
before(async () => {
  if (!baseUrl) return;
  url = await freshDatabase("typedpg_protocol");
  const admin = new pg.Client({ connectionString: url });
  await admin.connect();
  await admin.query("INSERT INTO users (name, mood) VALUES ('Ana', 'happy'), ('Bia', 'sad')");
  mood = Number((await admin.query("SELECT 'mood'::regtype::oid AS oid")).rows[0].oid);
  await admin.end();
  wire = await Wire.start(url);
});
after(() => wire?.close());

interface Driver {
  connect(url: string): Promise<{ db: Executor; close(): Promise<void> }>;
  /** The round trips a query with parameters takes. */
  roundTrips: number;
  /** What a Parse declares for parameters of types `inferred`. */
  declared(inferred: number[]): number[];
}

const drivers: [string, Driver][] = [
  [
    "node-postgres",
    {
      async connect(url) {
        const client = new pg.Client({ connectionString: url });
        await client.connect();
        return { db: client, close: () => client.end() };
      },
      roundTrips: 1,
      // No type at all: each is unspecified.
      declared: () => [],
    },
  ],
  [
    "postgres.js",
    {
      async connect(url) {
        const sql = postgres(url, { max: 1, onnotice: () => {} });
        // Connect (and fetch the array types) before anything is counted.
        await sql`SELECT 1`;
        return { db: sql, close: () => sql.end() };
      },
      roundTrips: 2,
      declared: (inferred) => inferred.map(() => UNKNOWN),
    },
  ],
];

for (const [name, driver] of drivers) {
  describe(name, { skip: !baseUrl && "DATABASE_URL is not set" }, () => {
    let db: Executor;
    let close: () => Promise<void>;
    before(async () => {
      ({ db, close } = await driver.connect(wire.url));
    });
    after(() => close());

    /**
     * `sent` is one query, on the unnamed statement, whose parameters the
     * server inferred as `inferred` (none: no parameter).
     */
    function assertSent(sent: Sent, inferred: number[]) {
      if (inferred.length === 0) {
        // The simple query protocol: one Query message.
        assert.deepEqual(sent.flights, ["Q"]);
        return;
      }
      assert.equal(sent.roundTrips, driver.roundTrips, `round trips: ${JSON.stringify(sent.flights)}`);
      assert.equal(sent.syncs, 1);
      assert.deepEqual(sent.parses, [{ name: "", oids: driver.declared(inferred) }]);
      assert.equal(sent.describedFirst, driver.roundTrips > 1);
      // What the server inferred, when the driver asked: the casts' types.
      if (sent.describedFirst) assert.deepEqual(sent.inferred, [inferred]);
    }

    test("no parameter", async () => {
      const [n, sent] = await wire.sent(() => q.countUsers.fetchValue(db));
      assert.equal(n, 2n);
      assertSent(sent, []);
    });

    test("an int parameter", async () => {
      const [user, sent] = await wire.sent(() => q.userById.fetchOne(db, { id: 2 }));
      assert.equal(user.name, "Bia");
      assertSent(sent, [INT4]);
    });

    test("an enum parameter: inferred as the enum, not text", async () => {
      const [rows, sent] = await wire.sent(() => q.usersByMood.fetchAll(db, { mood: "sad" }));
      assert.deepEqual(rows, [{ id: 2, name: "Bia", mood: "sad" }]);
      assertSent(sent, [mood]);
    });

    test("an enum list spread", async () => {
      const [rows, sent] = await wire.sent(() => q.usersWithMoods.fetchAll(db, { moods: ["happy", "sad"], after: 0 }));
      assert.deepEqual(rows, [{ name: "Ana" }, { name: "Bia" }]);
      assertSent(sent, [INT4, mood, mood]);
    });

    test("a jsonb parameter, encoded once", async () => {
      const doc = { a: [1, 'quo"te', null], "b\\": { c: "ü\n" } };
      const [value, sent] = await wire.sent(() => q.jsonbEcho.fetchValue(db, { doc }));
      assert.deepEqual(value, doc);
      assertSent(sent, [JSONB]);
    });

    test("a timestamptz parameter", async () => {
      const at = new Date("1999-06-15T12:00:00.123Z");
      const [year, sent] = await wire.sent(() => q.yearOf.fetchValue(db, { at }));
      assert.equal(year, "1999");
      assertSent(sent, [TIMESTAMPTZ]);
    });

    test("execute", async () => {
      const [count, sent] = await wire.sent(() => q.touch.execute(db, { by: 1n, ids: [1, 2] }));
      assert.equal(count, 2);
      assertSent(sent, [INT8, INT4_ARRAY]);
    });

    test("ten executions: never a prepared statement", async () => {
      const [, sent] = await wire.sent(async () => {
        for (let i = 0; i < 10; i++) await q.userById.fetchOne(db, { id: 1 + (i % 2) });
      });
      assert.equal(sent.roundTrips, 10 * driver.roundTrips);
      assert.equal(sent.syncs, 10);
      assert.deepEqual(
        sent.parses.map((p) => p.name),
        Array(10).fill(""),
      );
    });
  });
}
