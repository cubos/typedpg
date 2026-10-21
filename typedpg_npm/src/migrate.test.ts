import assert from "node:assert/strict";
import { test } from "node:test";

import { embedMigrations } from "./migrate.js";

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

test("two migrations of the same version keep a deterministic order", () => {
  // The name settles it, so the runner and the compiler agree.
  const set = embedMigrations([
    ["1_b", "SELECT 1;", null],
    ["1_a", "SELECT 1;", null],
  ]);
  assert.deepEqual(
    set.migrations.map((m) => m.name),
    ["1_a", "1_b"],
  );
});

test("a version of any length is compared as a number", () => {
  const huge = `${"9".repeat(40)}_x`;
  const set = embedMigrations([
    [huge, "SELECT 1;", null],
    ["10_x", "SELECT 1;", null],
  ]);
  assert.deepEqual(
    set.migrations.map((m) => m.name),
    ["10_x", huge],
  );
});
