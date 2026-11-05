// The errors `typedpg check` rejects, which `typedpg lint` reports as
// failures too: a query and a COPY target that do not analyze.
import pg from "pg";
import { copyIn, sql } from "./db.ts";

const pool = new pg.Pool();

export async function broken() {
  // @ts-expect-error: column "nope" does not exist
  await sql("SELECT nope FROM users").fetchAll(pool);
  // @ts-expect-error: relation "missing" does not exist
  await copyIn("missing (id)").execute(pool, []);
}
