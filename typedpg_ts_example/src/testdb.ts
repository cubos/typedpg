// Test support: a fresh database per test file (`node --test` runs files in
// parallel), migrated with the module's embedded migrations, and the
// drivers the tests run on.

import pg from "pg";
import postgres from "postgres";
import { type Executor, migrate } from "typedpg";

import { migrations } from "./db.ts";

export const baseUrl = process.env.DATABASE_URL;

/** `name`'s URL: the base URL with another database. */
export function urlFor(name: string): string {
  const u = new URL(baseUrl!);
  u.pathname = `/${name}`;
  return u.toString();
}

/** Drop and create database `name`; with `migrated`, run the migrations. */
export async function freshDatabase(name: string, migrated = true): Promise<string> {
  const admin = new pg.Client({ connectionString: baseUrl });
  await admin.connect();
  await admin.query(`DROP DATABASE IF EXISTS "${name}" WITH (FORCE)`);
  await admin.query(`CREATE DATABASE "${name}"`);
  await admin.end();
  const url = urlFor(name);
  if (migrated) {
    const pool = new pg.Pool({ connectionString: url });
    await migrate(pool, migrations);
    await pool.end();
  }
  return url;
}

export interface Connection {
  db: Executor;
  close(): Promise<void>;
}

/** The drivers every runtime test runs on. */
export const drivers: [string, (url: string) => Promise<Connection>][] = [
  [
    "pg Pool",
    async (url) => {
      const pool = new pg.Pool({ connectionString: url, max: 4 });
      return { db: pool, close: () => pool.end() };
    },
  ],
  [
    "pg Client",
    async (url) => {
      const client = new pg.Client({ connectionString: url });
      await client.connect();
      return { db: client, close: () => client.end() };
    },
  ],
  [
    "postgres.js",
    async (url) => {
      const sql = postgres(url, { onnotice: () => {}, max: 4 });
      return { db: sql, close: () => sql.end() };
    },
  ],
];

/** A deterministic PRNG (mulberry32), so a failing fuzz case reproduces. */
export function prng(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
