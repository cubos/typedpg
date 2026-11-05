// The lint fixture: `typedpg lint` reports these, pinned by
// ../expected.stdout. The clean query and the two allow comments report
// nothing.
import pg from "pg";
import { sql } from "./db.ts";

const pool = new pg.Pool();

export async function cases() {
  // null_comparison, an error.
  await sql("SELECT id FROM users WHERE email = NULL").fetchAll(pool);
  // The same mistake, allowed for the whole query.
  await sql("-- typedpg-lint: allow(null_comparison)\nSELECT id FROM users WHERE email = NULL").fetchAll(pool);
  // The safe spelling.
  await sql("SELECT id FROM users WHERE email IS NULL").fetchAll(pool);
  // unbounded_write, allowed through a block comment.
  await sql("/* typedpg-lint: allow(unbounded_write) */ UPDATE users SET name = 'x'").execute(pool);
}
