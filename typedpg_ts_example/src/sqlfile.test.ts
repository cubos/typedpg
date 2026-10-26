// The queries of a .sql file (src/sql/users.sql), through the module
// `typedpg gen` writes beside it, on a live PostgreSQL through each driver.

import assert from "node:assert/strict";
import { after, before, describe, test } from "node:test";

import type { Params, Row } from "@cubos/typedpg";

import * as u from "./sql/users.ts";
import { type Connection, baseUrl, drivers, freshDatabase } from "./testdb.ts";

// The named types are the queries' (checked by tsc, see `npm run typecheck`).
const params: u.InsertUserParams = { name: "Ana", email: null, mood: "happy", prefs: { theme: "dark" } };
const sameParams: Params<typeof u.insertUser> = params;
type ById = Row<typeof u.userById>;
const row: ById = { id: 1, name: "Ana", email: null, mood: null, prefs: null } satisfies u.UserByIdRow;

let url: string;
before(async () => {
  if (baseUrl) url = await freshDatabase("typedpg_sqlfile");
});

for (const [name, connect] of drivers) {
  describe(name, { skip: !baseUrl && "DATABASE_URL is not set" }, () => {
    let c: Connection;
    before(async () => {
      c = await connect(url);
    });
    after(() => c.close());

    test("insert, select, spreads, update", async () => {
      const id = await u.insertUser.fetchValue(c.db, sameParams);
      const other = await u.insertUser.fetchValue(c.db, { ...params, name: "Bia", mood: null, prefs: null });
      assert.deepEqual(await u.userById.fetchOne(c.db, { id }), {
        id,
        name: "Ana",
        email: null,
        mood: "happy",
        prefs: { theme: "dark" },
      });
      assert.equal(await u.userById.fetchOptional(c.db, { id: -1 }), null);
      assert.deepEqual(await u.usersByIds.fetchAll(c.db, { ids: [other, id] }), [{ name: "Ana" }, { name: "Bia" }]);
      assert.deepEqual(await u.usersByIds.fetchAll(c.db, { ids: [] }), []);
      assert.deepEqual(
        await u.insertPosts.fetchAll(c.db, { posts: [{ author: id, title: "a" }, { author: other, title: "b" }] }),
        [{ title: "a" }, { title: "b" }],
      );
      assert.equal(await u.renameUser.execute(c.db, { id, name: "Ana Maria" }), 1);
      const count: bigint = await u.countUsers.fetchValue(c.db);
      assert.ok(count >= 2n);
      assert.equal(row.id, 1);
    });
  });
}
