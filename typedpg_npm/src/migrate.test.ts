import assert from "node:assert/strict";
import { test } from "node:test";

import { embedMigrations, MigrationError } from "./migrate.js";

test("migrations are ordered by their numeric prefix", () => {
  // The prefix is a number, not text: `10_c` is a later migration than
  // `2_b`.
  const set = embedMigrations([
    ["10_c", "SELECT 1;", null],
    ["2_b", "SELECT 1;", null],
    ["1_a", "SELECT 1;", null],
    ["20_d", "SELECT 1;", null],
  ]);
  assert.deepEqual(
    set.migrations.map((m) => m.name),
    ["1_a", "2_b", "10_c", "20_d"],
  );
});

test("the migrations table is a qualified identifier", () => {
  // The same names the Rust runner accepts (`MigrationsConfig::validate`):
  // a name or a schema.name, each part unquoted or double-quoted.
  for (const table of [
    "public._migrations",
    "_migrations",
    '"my schema".log',
    '"my-schema"."my-migrations"',
    '"a""b".c',
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
  ]) {
    assert.throws(
      () => embedMigrations([], { table }),
      MigrationError,
      `the table ${JSON.stringify(table)} must be rejected`,
    );
  }
});
