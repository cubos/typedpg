import assert from "node:assert/strict";
import { test } from "node:test";

import { embedMigrations, MigrationError } from "./index.js";

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
  for (const table of ["public; DROP TABLE users --", "a.b.c", "", '"unclosed.log', '""']) {
    assert.throws(
      () => embedMigrations([], { table }),
      MigrationError,
      `the table ${JSON.stringify(table)} must be rejected`,
    );
  }
});
