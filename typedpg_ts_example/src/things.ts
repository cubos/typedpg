// Queries over the hard types of migrations/0002_types.sql: domains,
// enums, composites nested in arrays, ranges, multiranges, intervals,
// hstore. The type-level checks at the bottom fail tsc if an inferred type
// is not exactly the expected one.

import type { Interval, Params, Range, Row } from "@cubos/typedpg";
import { copyIn, sql } from "./db.ts";
import type { Point2 } from "./domains.ts";

export const insertThing = sql(`
  INSERT INTO things (label, email, qty, tags, colors, shape, shape_d, shapes, span, periods,
                      amounts, dates, dur, attrs, big, bigs, nums, flags, blobs, stamps, docs,
                      day, at_local, clock, id_uuid, addr, money_v, bits, ch)
  VALUES ($label, $email, $qty, $tags, $colors, $shape, $shape_d, $shapes, $span, $periods,
          $amounts, $dates, $dur, $attrs, $big, $bigs, $nums, $flags, $blobs, $stamps, $docs,
          $day, $at_local, $clock, $id_uuid, $addr, $money_v, $bits, $ch)
  RETURNING id
`);

export const thingById = sql("SELECT * FROM things WHERE id = $id");

/** Every column, and PG's own JSON rendering of the row: the decoding oracle. */
export const thingWithJson = sql("SELECT t.*, to_jsonb(t) AS j FROM things t WHERE t.id = $id");

export const constructed = sql(`
  SELECT ROW(1, 'a', NULL::text) AS r,
         ARRAY[ROW(1, 2.5)::point2] AS arr_of_row,
         ARRAY['', NULL, 'NULL', '"', '\\', ',', '{}', ' a ', 'é'] AS strings,
         int4range(1, 10, '[]') AS discrete,
         '(,5]'::numrange AS unbounded,
         'empty'::int4range AS nothing,
         '{[1,3), [5,7)}'::int4multirange AS multi,
         interval '1 year 2 mons -3 days 04:05:06.789' AS iv,
         '"a"=>"1", "b c"=>NULL'::hstore AS h
`);

export const copyThings = copyIn("things (label, tags, colors, shape, span, attrs, big, dur)");

export const streamThings = sql("SELECT id, label FROM things ORDER BY id");

export const labelsByColor = sql("SELECT label FROM things WHERE $color = ANY (colors) ORDER BY id");

// ── type-level checks ──

type Equal<A, B> = (<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2 ? true : false;
type Expect<T extends true> = T;

type Color = "red" | "green" | "with space" | 'quo"te' | "back\\slash" | "NULL";
type Shape = {
  name: string | null;
  color: Color | null;
  points: (Point2 | null)[] | null;
  tags: (string | null)[] | null;
  meta: import("@cubos/typedpg").JsonValue | null;
};

export type Checks = [
  Expect<
    Equal<
      Row<typeof thingById>,
      {
        id: number;
        label: string;
        email: string | null;
        qty: number | null;
        tags: (string | null)[] | null;
        colors: (Color | null)[] | null;
        shape: Shape | null;
        shape_d: Shape | null;
        shapes: (Shape | null)[] | null;
        span: Range<number> | null;
        periods: Range<Date>[] | null;
        amounts: Range<string> | null;
        dates: Range<string> | null;
        dur: Interval | null;
        attrs: Record<string, string | null> | null;
        big: bigint | null;
        bigs: (bigint | null)[] | null;
        nums: (string | null)[] | null;
        flags: (boolean | null)[] | null;
        blobs: (Uint8Array | null)[] | null;
        stamps: (Date | null)[] | null;
        docs: (import("@cubos/typedpg").JsonValue | null)[] | null;
        day: string | null;
        at_local: Date | null;
        clock: string | null;
        id_uuid: string | null;
        addr: string | null;
        money_v: string | null;
        bits: string | null;
        ch: string | null;
      }
    >
  >,
  // Parameters take the looser input types.
  Expect<Equal<Params<typeof insertThing>["big"], bigint | number | string | null>>,
  Expect<Equal<Params<typeof insertThing>["dur"], Interval | string | null>>,
  // A timestamp is a Date in and out.
  Expect<Equal<Params<typeof insertThing>["stamps"], readonly (Date | null)[] | null>>,
  Expect<Equal<Params<typeof insertThing>["at_local"], Date | null>>,
  // A composite parameter: the fields' input types.
  Expect<
    Equal<
      Params<typeof insertThing>["shape"],
      {
        name: string | null;
        color: Color | null;
        points: readonly (Point2 | null)[] | null;
        tags: readonly (string | null)[] | null;
        meta: unknown;
      } | null
    >
  >,
  Expect<Equal<Params<typeof labelsByColor>, { color: Color }>>,
  Expect<
    Equal<
      Row<typeof constructed>,
      {
        r: { f1: number; f2: string; f3: string | null };
        arr_of_row: Point2[];
        strings: (string | null)[];
        discrete: Range<number>;
        unbounded: Range<string>;
        nothing: Range<number>;
        multi: Range<number>[];
        iv: Interval;
        h: Record<string, string | null>;
      }
    >
  >,
];
