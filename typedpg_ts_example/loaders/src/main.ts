// The queries of users.sql, imported as the .sql file itself, run on
// DATABASE_URL through whichever loader compiled it (the Node hook, a
// bundle, Bun's plugin); prints what they return, which the e2e compares.
import pg from "pg";
import { loadMigrations, migrate } from "@cubos/typedpg";
import { countUsers, insertUser, userById } from "./users.sql";

const pool = new pg.Pool({ connectionString: process.env.DATABASE_URL });
try {
  await migrate(pool, loadMigrations("../migrations"));
  await pool.query("TRUNCATE users, posts RESTART IDENTITY CASCADE");
  const id = await insertUser.fetchValue(pool, { name: "Ana", prefs: { theme: "dark" } });
  const user = await userById.fetchOne(pool, { id });
  const email: string | null = user.email;
  const count: bigint = await countUsers.fetchValue(pool);
  console.log(JSON.stringify({ user, email, count: String(count) }));
} finally {
  await pool.end();
}

// Never called: the types come from the .sql file.
export async function typeErrors() {
  // @ts-expect-error: a string for an int4
  await userById.fetchOne(pool, { id: "1" });
  // @ts-expect-error: `name` is NOT NULL
  const name: null = (await userById.fetchOne(pool, { id: 1 })).name;
  return name;
}
