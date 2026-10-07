// Runs the example's queries on a live PostgreSQL through each supported
// driver. Needs DATABASE_URL (an empty database the migrations are applied
// to); `e2e.sh` starts one in Docker.

import assert from "node:assert/strict";
import { after, before, describe, test } from "node:test";

import pg from "pg";
import postgres from "postgres";
import { NoRowsError, TooManyRowsError, TypedpgError, type Executor } from "@cubos/typedpg";

import * as q from "./queries.ts";
import { baseUrl, drivers, freshDatabase } from "./testdb.ts";

// The application's own parsers must not change what typedpg decodes.
pg.types.setTypeParser(20, (v) => Number.parseInt(v, 10));
pg.types.setTypeParser(1184, () => "not a date");

let url: string;
before(async () => {
  if (baseUrl) url = await freshDatabase("typedpg_e2e");
});

for (const [name, connect] of drivers) {
  describe(name, { skip: !baseUrl && "DATABASE_URL is not set" }, () => {
    let db: Executor;
    let close: () => Promise<void>;
    before(async () => {
      ({ db, close } = await connect(url));
      const admin = new pg.Client({ connectionString: url });
      await admin.connect();
      await admin.query("TRUNCATE users, posts RESTART IDENTITY CASCADE");
      await admin.end();
    });
    after(() => close());

    test("spread insert, then every column type back", async () => {
      const inserted = await q.insertUsers.fetchAll(db, {
        users: [
          {
            name: "Ana",
            email: "ana@example.com",
            mood: "happy",
            tags: ["a", 'quo"te', null, "back\\slash", "comma,brace}"],
            prefs: { theme: "dark", fontSize: 14 },
            visits: 2n,
            avatar: new Uint8Array([0, 1, 255]),
          },
          { name: "Bia", email: null, mood: null, tags: [], prefs: null, visits: 0, avatar: null },
          { name: "Caio", email: null, mood: "sad", tags: [], prefs: null, visits: 9007199254740993n, avatar: null },
        ],
      });
      assert.deepEqual(inserted, [
        { id: 1, name: "Ana" },
        { id: 2, name: "Bia" },
        { id: 3, name: "Caio" },
      ]);

      const ana = await q.userById.fetchOne(db, { id: 1 });
      assert.ok(ana.created_at instanceof Date && !Number.isNaN(ana.created_at.getTime()));
      assert.ok(Math.abs(ana.created_at.getTime() - Date.now()) < 60_000);
      assert.deepEqual(
        { ...ana, created_at: null },
        {
          id: 1,
          name: "Ana",
          email: "ana@example.com",
          mood: "happy",
          tags: ["a", 'quo"te', null, "back\\slash", "comma,brace}"],
          prefs: { theme: "dark", fontSize: 14 },
          balance: "0.00",
          visits: 2n,
          avatar: new Uint8Array([0, 1, 255]),
          home: null,
          created_at: null,
        },
      );
      // Beyond 2^53: a bigint, exact.
      assert.equal((await q.userById.fetchOne(db, { id: 3 })).visits, 9007199254740993n);
    });

    test("empty spread runs nothing", async () => {
      assert.deepEqual(await q.insertUsers.fetchAll(db, { users: [] }), []);
    });

    test("fetchOne / fetchOptional / fetchValue", async () => {
      assert.equal(await q.countUsers.fetchValue(db), 3n);
      assert.deepEqual(await q.userNames.fetchAll(db), [{ name: "Ana" }, { name: "Bia" }, { name: "Caio" }]);
      await assert.rejects(q.userNames.fetchOne(db), TooManyRowsError);
      await assert.rejects(q.userNames.fetchOptional(db), TooManyRowsError);
      await assert.rejects(q.userById.fetchOne(db, { id: 999 }), NoRowsError);
      assert.equal(await q.userById.fetchOptional(db, { id: 999 }), null);
      assert.equal(await q.userEmail.fetchValueOptional(db, { id: 999 }), null);
      assert.equal(await q.userEmail.fetchValueOptional(db, { id: 2 }), null);
      assert.equal(await q.userEmail.fetchValue(db, { id: 1 }), "ana@example.com");
    });

    test("enum parameter, narrowed column", async () => {
      assert.deepEqual(await q.usersByMood.fetchAll(db, { mood: "sad" }), [{ id: 3, name: "Caio", mood: "sad" }]);
    });

    test("execute returns the affected rows", async () => {
      assert.equal(await q.touch.execute(db, { by: 5n, ids: [1, 2] }), 2);
      assert.equal((await q.userById.fetchOne(db, { id: 1 })).visits, 7n);
    });

    test("IN a list spread, empty lists included", async () => {
      const names = (rows: { name: string }[]) => rows.map((r) => r.name);
      assert.deepEqual(names(await q.usersIn.fetchAll(db, { ids: [1, 3] })), ["Ana", "Caio"]);
      assert.deepEqual(names(await q.usersNotIn.fetchAll(db, { ids: [1, 3] })), ["Bia"]);
      // The query runs: `IN` an empty list is false, `NOT IN` it true.
      assert.deepEqual(await q.usersIn.fetchAll(db, { ids: [] }), []);
      assert.deepEqual(names(await q.usersNotIn.fetchAll(db, { ids: [] })), ["Ana", "Bia", "Caio"]);
      // Enum elements, and a regular parameter numbered before them.
      assert.deepEqual(
        names(await q.usersWithMoods.fetchAll(db, { moods: ["happy", "sad"], after: 1 })),
        ["Caio"],
      );
    });

    test("an int8 parameter takes a string, exactly", async () => {
      assert.equal(await q.touch.execute(db, { by: "-9007199254740993", ids: [3] }), 1);
      assert.equal((await q.userById.fetchOne(db, { id: 3 })).visits, 0n);
      assert.equal(await q.touch.execute(db, { by: "9007199254740993", ids: [3] }), 1);
      assert.equal((await q.userById.fetchOne(db, { id: 3 })).visits, 9007199254740993n);
    });

    test("records, joins, aggregates", async () => {
      const p1 = await q.insertPost.fetchValue(db, { author: 1, title: "Hello", body: null });
      const p2 = await q.insertPost.fetchValue(db, { author: 1, title: "World", body: "text" });
      assert.deepEqual([p1, p2], [1, 2]);
      assert.deepEqual(await q.postsWithAuthor.fetchAll(db), [
        { title: "Hello", author: "Ana", body: null, who: { f1: 1, f2: "ana@example.com" } },
        { title: "World", author: "Ana", body: "text", who: { f1: 1, f2: "ana@example.com" } },
      ]);
      assert.deepEqual(await q.annotatedPosts.fetchAll(db), [
        { title: "Hello", author: "Ana" },
        { title: "World", author: "Ana" },
      ]);
      assert.deepEqual(await q.authorsWithPostCount.fetchAll(db), [
        { name: "Ana", posts: 2n, titles: ["Hello", "World"] },
        { name: "Bia", posts: 0n, titles: [null] },
        { name: "Caio", posts: 0n, titles: [null] },
      ]);
    });

    test("composite column round trip", async () => {
      const street = 'Rua "A", (1)\\ok';
      assert.equal(await q.setHome.execute(db, { street, number: 10, id: 2 }), 1);
      assert.deepEqual(await q.homeOf.fetchOne(db, { id: 2 }), { home: { street, number: 10 }, street });
    });

    test("scalars", async () => {
      const row = await q.valuesQuery.fetchOne(db);
      assert.equal(row.f, 1.5);
      assert.ok(Number.isNaN(row.nan));
      assert.ok(row.now instanceof Date);
      assert.deepEqual(row.b, new Uint8Array([0, 255]));
      assert.equal(row.nothing, null);
    });

    // Last: it adds posts.
    test("rows spreads with no item still run the query", async () => {
      // Two inserts in CTEs, the first one empty: the second still inserts.
      const posts = [
        { author: 1, title: "x" },
        { author: 2, title: "y" },
      ];
      assert.deepEqual(await q.insertPostBatches.fetchOne(db, { first: [], second: posts }), { a: 0n, b: 2n });
      // A written row and an empty spread: the written row is inserted.
      assert.deepEqual(await q.insertPostsAfterOne.fetchAll(db, { author: 1, more: [] }), [{ title: "first" }]);
      // An aggregate over an empty VALUES has its row.
      assert.equal(await q.countTitles.fetchValue(db, { titles: [] }), 0n);
    });
  });
}

describe("pg transaction client", { skip: !baseUrl && "DATABASE_URL is not set" }, () => {
  test("queries run on the transaction, and a stale schema is caught", async () => {
    const pool = new pg.Pool({ connectionString: url });
    const client = await pool.connect();
    try {
      await client.query("BEGIN");
      await q.insertUsers.execute(client, {
        users: [{ name: "Tx", email: null, mood: null, tags: [], prefs: null, visits: 0, avatar: null }],
      });
      assert.equal(await q.countUsers.fetchValue(client), 4n);
      // `SELECT *` after a migration the generated module hasn't seen.
      await client.query("ALTER TABLE users ADD COLUMN extra int4");
      await assert.rejects(q.userById.fetchOne(client, { id: 1 }), (e: unknown) => {
        assert.ok(e instanceof TypedpgError);
        assert.match(e.message, /are not the ones it was generated with.*run `typedpg gen`/);
        return true;
      });
      await client.query("ROLLBACK");
      assert.equal(await q.countUsers.fetchValue(pool), 3n);
    } finally {
      client.release();
      await pool.end();
    }
  });
});

describe("postgres.js transaction", { skip: !baseUrl && "DATABASE_URL is not set" }, () => {
  test("queries run on the transaction", async () => {
    const sql = postgres(url, { onnotice: () => {} });
    try {
      await sql
        .begin(async (tx) => {
          await q.insertUsers.execute(tx, {
            users: [{ name: "Tx", email: null, mood: null, tags: [], prefs: null, visits: 0, avatar: null }],
          });
          assert.equal(await q.countUsers.fetchValue(tx), 4n);
          throw new Error("rollback");
        })
        .catch((e: Error) => assert.equal(e.message, "rollback"));
      assert.equal(await q.countUsers.fetchValue(sql), 3n);
    } finally {
      await sql.end();
    }
  });
});
