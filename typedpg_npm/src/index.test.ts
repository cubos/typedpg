import assert from "node:assert/strict";
import { test } from "node:test";

import { type Driver, createCopyIn, createQuery, embedMigrations, MigrationError } from "./index.js";

test("the migrations table is a qualified identifier", () => {
  // The same names the Rust runner accepts (`MigrationsConfig::validate`):
  // a name or a `schema.name`, each part unquoted or double-quoted.
  for (const table of [
    "public._migrations",
    "_migrations",
    '"my schema".log',
    '"my-schema"."my-migrations"',
    '"a""b".c',
    // A quoted part may hold a digit, and a part may be quoted on its own.
    '"1abc"',
    'a."b"',
    '"a".b',
    "A.B",
    '""""',
  ]) {
    const set = embedMigrations([], { table });
    assert.equal(set.config.table, table);
  }
  // Not a qualified identifier.
  for (const table of [
    "public; DROP TABLE users --",
    "a.b.c",
    "",
    '"unclosed.log',
    '""',
    "1abc",
    "a..b",
    ".a",
    "a.",
    '"a"b".c',
    "my schema",
    '""a"',
    'a"b"',
  ]) {
    assert.throws(
      () => embedMigrations([], { table }),
      MigrationError,
      `the table ${JSON.stringify(table)} must be rejected`,
    );
  }
});

test("a spread's value that can't be encoded names the spread, item and field", async () => {
  const driver: Driver = {
    async typedpgQuery() {
      return { rows: [], rowCount: 0 };
    },
  };
  const rows = createQuery<{ params: { rows: { id: number; on: boolean }[] }; row: {} }>(
    "INSERT INTO t (id, on) VALUES $..rows { id, on }",
    JSON.stringify({
      sql: ["INSERT INTO t (id, on) ", ["values", [["rows", 0]], "SELECT NULL::int4, NULL::bool WHERE false"]],
      params: [],
      spreads: [["rows", [["id", "number", "::int4"], ["on", "bool", "::bool"]], "rows"]],
      columns: [],
    }),
  );
  await assert.rejects(rows.execute(driver, { rows: [{ id: 1, on: true }, { id: 2, on: "false" as never }] }), {
    name: "TypeError",
    message: "the spread `rows`, item 1, field `on`: typedpg: a bool parameter must be a boolean, not string",
  });
  await assert.rejects(rows.execute(driver, {} as never), {
    name: "TypeError",
    message: "the spread `rows` must be an array, not undefined",
  });

  const ids = createQuery<{ params: { ids: boolean[] }; row: {} }>(
    "SELECT 1 WHERE x IN $..ids",
    JSON.stringify({
      sql: ["SELECT 1 WHERE x", ["list", 0, "::bool", "(SELECT NULL::bool WHERE false)", " ", "IN "]],
      params: [],
      spreads: [["ids", [["", "bool", "::bool"]], "list"]],
      columns: [],
    }),
  );
  await assert.rejects(ids.execute(driver, { ids: [true, 1 as never] }), {
    name: "TypeError",
    message: "the spread `ids`: typedpg: a bool parameter must be a boolean, not number",
  });
});

test("a COPY row's value that can't be encoded names the row and column", async () => {
  let sent = "";
  const driver: Driver = {
    async typedpgQuery() {
      return { rows: [], rowCount: 0 };
    },
    async typedpgCopyIn(_sql, data) {
      for await (const chunk of data) sent += chunk;
      return 0;
    },
  };
  const copyIn = createCopyIn<{ "t (id, on)": { row: { id: number; on: boolean } } }>({
    "t (id, on)": JSON.stringify({ sql: "COPY t (id, on) FROM STDIN", columns: [["id", "number"], ["on", "bool"]] }),
  });
  await assert.rejects(copyIn("t (id, on)").execute(driver, [{ id: 1, on: true }, { id: 2, on: 0 as never }]), {
    name: "TypeError",
    message: "row 1, column `on`: typedpg: a bool parameter must be a boolean, not number",
  });
  assert.equal(sent, "");
});
