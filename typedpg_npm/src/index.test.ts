import assert from "node:assert/strict";
import { test } from "node:test";

import { embedMigrations, MigrationError } from "./index.js";

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
