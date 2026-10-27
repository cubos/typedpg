import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { driftMessage, hash, normalize, splitStatements } from "./core.js";

interface Case {
  a: string;
  b: string;
  same: boolean;
  why: string;
}

// The cases are shared with the Rust runner's own test: the two runners
// must agree on what drift is, or one database would look drifted to one
// CLI and not the other. Both now run the same code — this test drives the
// wasm module the runtime uses.
test("normalization cases are the shared ones", () => {
  const url = new URL("../../typedpg_core/tests/fixtures/normalize.json", import.meta.url);
  const cases = JSON.parse(readFileSync(url, "utf8")) as Case[];
  assert.ok(cases.length >= 25, `only ${cases.length} cases`);
  for (const c of cases) {
    assert.equal(hash(c.a) === hash(c.b), c.same, `${c.why}:\n  a = ${JSON.stringify(c.a)}\n  b = ${JSON.stringify(c.b)}`);
  }
});

test("non-ASCII words pass through unchanged", () => {
  // Both runners hash the normalized text's UTF-8 bytes; re-encoding the
  // bytes of a multi-byte character would give them different hashes. The
  // hash is pinned in the Rust runner's test too.
  assert.equal(normalize("SELECT Café FROM t;"), "select café from t;");
  assert.equal(hash("SELECT Café FROM t;"), "5a3c8a972c93614feb3618f2e3ce47b1");
});

test("splitStatements decodes multi-byte UTF-8 and keeps character offsets", () => {
  // The module reports byte offsets into the UTF-8 encoding; the wrapper
  // turns them back into offsets into the JavaScript string. `é` is two
  // bytes but one character, so a byte offset would put the second
  // statement at 12 instead of 11.
  const sql = "SELECT 'é'; SELECT '🙂';";
  const statements = splitStatements(sql);
  assert.deepEqual(
    statements.map(([, statement]) => statement),
    ["SELECT 'é';", " SELECT '🙂';"],
  );
  for (const [at, statement] of statements) {
    assert.equal(sql.slice(at, at + statement.length), statement);
  }
  assert.equal(statements[1]![0], 11);
});

test("a drift message from the bridge carries a unified diff hunk", () => {
  const message = driftMessage(
    "0001_a",
    "CREATE TABLE a (\n    id INT\n);",
    "CREATE TABLE a (\n    id BIGINT\n);",
    true,
  );
  assert.match(message, /^migration '0001_a' has been modified since it was applied:\n/);
  assert.match(message, /^--- applied$/m);
  assert.match(message, /^\+\+\+ 0001_a\.sql$/m);
  assert.match(message, /^@@ -1,3 \+1,3 @@$/m);
  assert.match(message, /^-    id INT$/m);
  assert.match(message, /^\+    id BIGINT$/m);
  assert.match(message, /fail_on_drift = false/);
  assert.match(message, /"failOnDrift": false/);
});
