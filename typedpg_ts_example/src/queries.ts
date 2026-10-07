import type { Row, Params } from "@cubos/typedpg";
import { sql } from "./db.ts";
import * as db from "./db.ts";
import type { UserPrefs } from "./domains.ts";

export const insertUsers = sql(`
  INSERT INTO users (name, email, mood, tags, prefs, visits, avatar)
  VALUES $..users { name, email, mood, tags, prefs, visits, avatar }
  RETURNING id, name
`);

export const userById = sql("SELECT * FROM users WHERE id = $id");

export const userNames = sql("SELECT name FROM users ORDER BY id");

export const userEmail = sql("SELECT email FROM users WHERE id = $id");

export const countUsers = db.sql("SELECT count(*) AS n FROM users");

export const usersByMood = sql("SELECT id, name, mood FROM users WHERE mood = $mood ORDER BY id");

export const touch = sql("UPDATE users SET visits = visits + $by WHERE id = ANY($ids)");

export const postsWithAuthor = sql(`
  SELECT p.title, u.name AS author, p.body, ROW(u.id, u.email) AS who
  FROM posts p JOIN users u ON u.id = p.author_id
  ORDER BY p.id
`);

// Nullability annotations on the aliases: the server names the columns
// `title!` / `author?`, the rows have `title` / `author`.
export const annotatedPosts = sql(
  'SELECT p.title AS "title!", u.name AS "author?" FROM posts p JOIN users u ON u.id = p.author_id ORDER BY p.id',
);

export const authorsWithPostCount = sql(`
  SELECT u.name, count(p.id) AS posts, array_agg(p.title ORDER BY p.id) AS titles
  FROM users u LEFT JOIN posts p ON p.author_id = u.id
  GROUP BY u.id
  ORDER BY u.id
`);

export const insertPost = sql(
  "INSERT INTO posts (author_id, title, body) VALUES ($author, $title, $body?) RETURNING id",
);

export const setHome = sql("UPDATE users SET home = ROW($street, $number) WHERE id = $id");

export const homeOf = sql("SELECT home, (home).street FROM users WHERE id = $id");

export const valuesQuery = sql(
  "SELECT 1.5::float8 AS f, 'NaN'::float8 AS nan, now() AS now, '\\x00ff'::bytea AS b, NULL::text AS nothing",
);

// ── type-level checks: `tsc` fails if an inferred type is not the expected one ──

type Equal<A, B> = (<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2 ? true : false;
type Expect<T extends true> = T;

export type Checks = [
  Expect<
    Equal<
      Row<typeof userById>,
      {
        id: number;
        name: string;
        email: string | null;
        mood: "happy" | "sad" | "neutral" | null;
        tags: (string | null)[];
        prefs: UserPrefs | null;
        balance: string;
        visits: bigint;
        avatar: Uint8Array | null;
        home: { street: string | null; number: number | null } | null;
        created_at: Date;
      }
    >
  >,
  Expect<Equal<Params<typeof userById>, { id: number }>>,
  Expect<Equal<Row<typeof countUsers>, { n: bigint }>>,
  Expect<Equal<Awaited<ReturnType<typeof countUsers.fetchValue>>, bigint>>,
  Expect<Equal<Awaited<ReturnType<typeof userEmail.fetchValueOptional>>, string | null>>,
  Expect<Equal<Params<typeof usersByMood>, { mood: "happy" | "sad" | "neutral" }>>,
  // An int8 parameter also takes a number or a string.
  Expect<Equal<Params<typeof touch>, { by: bigint | number | string; ids: readonly (number | null)[] }>>,
  Expect<Equal<Params<typeof insertPost>, { author: number; title: string; body: string | null }>>,
  Expect<
    Equal<
      Row<typeof postsWithAuthor>,
      { title: string; author: string; body: string | null; who: { f1: number; f2: string | null } }
    >
  >,
  // `count` is never NULL; `array_agg` over a LEFT JOIN is never NULL here
  // (a group always has a row), but its elements can be.
  Expect<
    Equal<Row<typeof authorsWithPostCount>, { name: string; posts: bigint; titles: (string | null)[] }>
  >,
  Expect<Equal<Row<typeof annotatedPosts>, { title: string; author: string | null }>>,
];
