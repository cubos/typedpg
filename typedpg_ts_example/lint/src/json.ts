// json_stringify_param: a JSON.stringify(…) passed as a jsonb value stores a
// JSON string, not the value. Pinned by ../expected.stdout; the call passing
// the value itself reports nothing.
import pg from "pg";
import { copyIn, sql } from "./db.ts";

const pool = new pg.Pool();
const doc = { title: "hello" };

export async function documents() {
  const insert = sql("INSERT INTO documents (id, body) VALUES ($id, $body)");
  await insert.execute(pool, { id: 1, body: JSON.stringify(doc) });
  await insert.execute(pool, { id: 2, body: doc });
  // An element of a jsonb[] parameter.
  await sql("UPDATE documents SET tags = $tags WHERE id = $id").execute(pool, { id: 1, tags: [JSON.stringify(doc)] });
  // A field of a rows spread's item.
  await sql("INSERT INTO documents (id, body) VALUES $..rows { id, body }").execute(pool, {
    rows: [{ id: 3, body: JSON.stringify(doc) }],
  });
  // A column of a COPY's rows.
  await copyIn("documents (id, body)").execute(pool, [{ id: 4, body: JSON.stringify(doc) }]);
}
