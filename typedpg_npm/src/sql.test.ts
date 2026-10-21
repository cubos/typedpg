import assert from "node:assert/strict";
import { test } from "node:test";

import { stripTrailingJunk } from "./sql.js";

test("a trailing semicolon is dropped", () => {
  assert.equal(stripTrailingJunk("SELECT 1;"), "SELECT 1");
  assert.equal(stripTrailingJunk("SELECT 1 ;"), "SELECT 1");
  assert.equal(stripTrailingJunk("SELECT 1;  \n"), "SELECT 1");
});

test("trailing comments are dropped", () => {
  assert.equal(stripTrailingJunk("SELECT 1 -- a note"), "SELECT 1");
  assert.equal(stripTrailingJunk("SELECT 1 -- a note\n"), "SELECT 1");
  assert.equal(stripTrailingJunk("SELECT 1; -- a note"), "SELECT 1");
  assert.equal(stripTrailingJunk("SELECT 1 /* a note */"), "SELECT 1");
  assert.equal(stripTrailingJunk("SELECT 1 /* a /* nested */ note */"), "SELECT 1");
  assert.equal(stripTrailingJunk("SELECT 1 /* a note */ -- another"), "SELECT 1");
});

test("a semicolon inside a literal is kept", () => {
  assert.equal(stripTrailingJunk("SELECT ';'"), "SELECT ';'");
  assert.equal(stripTrailingJunk("SELECT 'a; b'"), "SELECT 'a; b'");
  assert.equal(stripTrailingJunk('SELECT ";";'), 'SELECT ";"');
  assert.equal(stripTrailingJunk("SELECT $$a;$$"), "SELECT $$a;$$");
  assert.equal(stripTrailingJunk("SELECT $tag$a;$tag$"), "SELECT $tag$a;$tag$");
  assert.equal(stripTrailingJunk("SELECT E'a\\';b'"), "SELECT E'a\\';b'");
  assert.equal(stripTrailingJunk("SELECT '--'"), "SELECT '--'");
  assert.equal(stripTrailingJunk("SELECT '/*'"), "SELECT '/*'");
});

test("a query without junk is untouched", () => {
  assert.equal(stripTrailingJunk("SELECT 1"), "SELECT 1");
  assert.equal(stripTrailingJunk("SELECT 1 "), "SELECT 1");
  assert.equal(stripTrailingJunk(""), "");
});
