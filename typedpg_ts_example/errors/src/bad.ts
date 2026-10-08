// Each statement is an error, pinned by `@ts-expect-error` (tsc fails if one
// is not reported) and by the e2e's check of what `typedpg gen` prints
// (expected.stderr).
import pg from "pg";
import { copyIn, sql } from "./db.ts";

const pool = new pg.Pool();
const dynamic = "SELECT 1";

export async function cases() {
  // @ts-expect-error: column "nmae" does not exist
  await sql("SELECT id, nmae FROM users").fetchAll(pool);
  // @ts-expect-error: a string for an int4
  await sql("SELECT id FROM users WHERE id = $id").fetchOne(pool, { id: "1" });
  // @ts-expect-error: the parameters are missing
  await sql("SELECT id FROM users WHERE id = $id").fetchOne(pool);
  // @ts-expect-error: unknown parameter
  await sql("SELECT id FROM users WHERE id = $id").fetchOne(pool, { idd: 1 });
  // @ts-expect-error: a non-nullable parameter
  await sql("SELECT id FROM users WHERE id = $id").fetchOne(pool, { id: null });
  // @ts-expect-error: fetchValue needs a single column
  await sql("SELECT id, name FROM users").fetchValue(pool);
  // @ts-expect-error: not a literal, so not analyzed
  await sql(dynamic).fetchAll(pool);
  // @ts-expect-error: positional placeholder
  await sql("SELECT id FROM users WHERE id = $1").fetchAll(pool);
  // @ts-expect-error: a label the enum doesn't have
  await sql("SELECT id FROM users WHERE mood = $mood").fetchAll(pool, { mood: "angry" });
  // @ts-expect-error: an anonymous record can't be a parameter
  await sql("SELECT $r::record AS r").fetchAll(pool, { r: "(1)" });
  // @ts-expect-error: two columns named id
  await sql("SELECT u.id, p.id FROM users u JOIN posts p ON p.author_id = u.id").fetchAll(pool);
  // @ts-expect-error: a range is a typedpg Range, not a number
  await sql("SELECT id FROM things WHERE span @> $n").fetchAll(pool, { n: 5, extra: 1 });
  // @ts-expect-error: a number for an interval
  await sql("SELECT now() - $d AS t").fetchAll(pool, { d: 5 });
  // @ts-expect-error: fetchStream checks its parameters too
  for await (const _ of sql("SELECT id FROM users WHERE id = $id").fetchStream(pool, { id: "x" })) break;
  // @ts-expect-error: COPY into a column that doesn't exist
  await copyIn("users (id, nope)").execute(pool, []);
  // @ts-expect-error: a COPY row of the wrong type
  await copyIn("users (name, email)").execute(pool, [{ name: 1, email: null }]);
  // @ts-expect-error: a template tag, not a function call
  await sql`SELECT 1`.fetchAll(pool);
}
