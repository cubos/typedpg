import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { hash, normalize, unifiedDiff } from "./normalize.js";

interface Case {
  a: string;
  b: string;
  same: boolean;
  why: string;
}

// The cases are shared with the Rust runner's own test: the two runners
// must agree on what drift is, or one database would look drifted to one
// CLI and not the other.
test("normalization cases are the shared ones", () => {
  const url = new URL("../../typedpg/tests/fixtures/normalize.json", import.meta.url);
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

test("a unified diff keeps three lines of context", () => {
  const applied = Array.from({ length: 10 }, (_, i) => `line ${i + 1}`).join("\n") + "\n";
  const file = applied.replace("line 5\n", "LINE 5\n");
  assert.equal(
    unifiedDiff(applied, file, "0001_a.sql"),
    [
      "--- applied",
      "+++ 0001_a.sql",
      "@@ -2,7 +2,7 @@",
      " line 2",
      " line 3",
      " line 4",
      "-line 5",
      "+LINE 5",
      " line 6",
      " line 7",
      " line 8",
    ].join("\n"),
  );
});

test("an unchanged file has no diff", () => {
  assert.equal(unifiedDiff("SELECT 1;\n", "SELECT 1;\n", "0001_a.sql"), "");
});

test("an empty applied file is an insertion from line 0", () => {
  assert.equal(
    unifiedDiff("", "a\nb\n", "0001_a.sql"),
    ["--- applied", "+++ 0001_a.sql", "@@ -0,0 +1,2 @@", "+a", "+b"].join("\n"),
  );
});

test("far-apart changes are separate hunks", () => {
  const applied = Array.from({ length: 20 }, (_, i) => `line ${i + 1}`).join("\n");
  const file = applied.replace("line 2", "LINE 2").replace("line 19", "LINE 19");
  const diff = unifiedDiff(applied, file, "0001_a.sql");
  assert.equal(diff.match(/^@@ /gm)?.length, 2);
  assert.match(diff, /^--- applied\n\+\+\+ 0001_a\.sql\n@@ -1,5 \+1,5 @@\n/);
  assert.match(diff, /\n@@ -16,5 \+16,5 @@\n/);
});

test("a small change to a 15k-line file stays a small diff", () => {
  const lines = Array.from({ length: 15_000 }, (_, i) => `line ${i + 1}`);
  const applied = lines.join("\n");
  const file = [...lines.slice(0, 10_000), "changed", ...lines.slice(10_001)].join("\n");
  const diff = unifiedDiff(applied, file, "big.sql");
  assert.match(diff, /^--- applied\n\+\+\+ big\.sql\n@@ -9998,7 \+9998,7 @@\n/);
  assert.equal(diff.split("\n").length, 3 + 8);
});
