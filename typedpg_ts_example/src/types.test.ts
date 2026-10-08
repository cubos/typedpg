// The hard types on a live PostgreSQL, through every driver: fixed values
// with a known canonical form, PostgreSQL's own JSON rendering of each row
// (`to_jsonb`) as an oracle for the nested ones, and a seeded fuzz of
// strings that stress every quoting rule (arrays in composites in arrays,
// hstore, enums with quotes).

import assert from "node:assert/strict";
import { after, before, describe, test } from "node:test";

import { type CopyRow, NoRowsError, type Params, range, emptyRange } from "@cubos/typedpg";

import { type Connection, baseUrl, drivers, freshDatabase, prng } from "./testdb.ts";
import * as t from "./things.ts";

type ThingParams = Params<typeof t.insertThing>;

const shape = {
  name: 'sq "uare" (1,2) {x}',
  color: 'quo"te' as const,
  points: [{ x: 1.5, y: -2 }, null, { x: null, y: 3 }],
  tags: ["a", null, "", "NULL", "back\\slash"],
  meta: { k: [1, "two", { deep: "va\"l" }] },
};

const fixed: ThingParams = {
  label: "fixed",
  email: "a@b.c",
  qty: 5,
  tags: ["x", null, "a,b", '"q"', "\\", "", "NULL", " sp ", "{}", "é", "tab\there", "line\nbreak"],
  colors: ["red", "with space", 'quo"te', "back\\slash", "NULL", null],
  shape,
  shape_d: { ...shape, name: "domain" },
  shapes: [shape, null, { name: null, color: null, points: null, tags: [], meta: null }],
  span: range(1, 10, "[]"),
  periods: [range(new Date("2024-01-01T00:00:00Z"), new Date("2024-02-01T12:30:00.5Z")), range(null, new Date(0))],
  amounts: range("1.5", null, "(]"),
  dates: range("2024-01-01", "2024-01-10", "[]"),
  dur: { months: 14, days: -3, microseconds: 14_706_789_000n },
  attrs: { a: "1", "b c": null, 'q"uote': "back\\slash", "": "empty key", "=>": ",," },
  big: 9223372036854775807n,
  bigs: [-9223372036854775808n, null, 0n],
  nums: ["1.50", null, "-0.000001", "NaN"],
  flags: [true, false, null],
  blobs: [new Uint8Array([0, 255, 92, 34]), null, new Uint8Array([])],
  stamps: [new Date("2024-02-29T12:34:56.789Z"), null, new Date("0044-03-15T12:00:00Z"), new Date(-62200000000000)],
  docs: [{ a: 1, "k\"ey": ["x", { y: null }] }, null, [1, "x"], "str", 42, true],
  day: "2024-02-29",
  at_local: "2024-02-29 12:34:56.789",
  clock: "04:05:06+02",
  id_uuid: "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
  addr: "192.168.0.1/24",
  money_v: "1234.5",
  bits: "1011",
  ch: "x",
};

/** What `fixed` reads back as: PostgreSQL's canonical forms. */
const fixedRow = {
  ...fixed,
  // Discrete ranges are canonicalized to `[)`; a multirange's ranges are
  // sorted.
  span: range(1, 11),
  periods: [range(null, new Date(0)), range(new Date("2024-01-01T00:00:00Z"), new Date("2024-02-01T12:30:00.5Z"))],
  dates: range("2024-01-01", "2024-01-11"),
  // An unbounded upper bound is never inclusive.
  amounts: range("1.5", null, "()"),
  money_v: "$1,234.50",
};

const strip = ({ id: _, ...rest }: Record<string, unknown>) => rest;

for (const [name, connect] of drivers) {
  describe(name, { skip: !baseUrl && "DATABASE_URL is not set" }, () => {
    let c: Connection;
    before(async () => {
      c = await connect(await freshDatabase(`typedpg_types_${name.replace(/\W/g, "_").toLowerCase()}`));
    });
    after(() => c.close());

    test("every hard type round-trips", async () => {
      const id = await t.insertThing.fetchValue(c.db, fixed);
      const row = await t.thingById.fetchOne(c.db, { id });
      assert.deepEqual(strip(row), fixedRow);
    });

    test("PostgreSQL's to_jsonb agrees with the decoded nested values", async () => {
      const id = await t.insertThing.fetchValue(c.db, fixed);
      const { j, ...row } = await t.thingWithJson.fetchOne(c.db, { id });
      const oracle = j as Record<string, unknown>;
      for (const col of ["label", "email", "qty", "tags", "colors", "shape", "shape_d", "shapes", "attrs", "flags", "docs"] as const) {
        assert.deepEqual(row[col], oracle[col], col);
      }
    });

    test("values PostgreSQL builds decode", async () => {
      assert.deepEqual(await t.constructed.fetchOne(c.db), {
        r: { f1: 1, f2: "a", f3: null },
        arr_of_row: [{ x: 1, y: 2.5 }],
        strings: ["", null, "NULL", '"', "\\", ",", "{}", " a ", "é"],
        discrete: range(1, 11),
        unbounded: range(null, "5", "(]"),
        nothing: emptyRange,
        multi: [range(1, 3), range(5, 7)],
        iv: { months: 14, days: -3, microseconds: 14_706_789_000n },
        h: { a: "1", "b c": null },
      });
    });

    test("seeded fuzz: nested quoting round-trips and matches to_jsonb", async () => {
      const random = prng(0x7e57);
      const pick = <T>(xs: readonly T[]): T => xs[Math.floor(random() * xs.length)]!;
      const pieces = ['"', "\\", ",", "{", "}", "(", ")", " ", "NULL", "null", "", "a", "é", "\t", "\n", "'", "=>", "😀", "[", "]", "\\\\", '""'];
      const str = () => Array.from({ length: Math.floor(random() * 5) }, () => pick(pieces)).join("");
      const maybe = <T>(f: () => T): T | null => (random() < 0.2 ? null : f());
      const arr = <T>(f: () => T, n = 4) => Array.from({ length: Math.floor(random() * n) }, f);
      const labels = ["red", "green", "with space", 'quo"te', "back\\slash", "NULL"] as const;
      const json = (depth = 0): unknown =>
        depth > 2 || random() < 0.4
          ? pick([str(), Math.round(random() * 1000) / 8, true, false])
          : random() < 0.5
            ? arr(() => json(depth + 1))
            : Object.fromEntries(arr(() => [str(), json(depth + 1)] as const));
      const randomShape = () => ({
        name: maybe(str),
        color: maybe(() => pick(labels)),
        points: maybe(() => arr(() => maybe(() => ({ x: maybe(() => random() * 100 - 50), y: random() })))),
        tags: maybe(() => arr(() => maybe(str))),
        meta: maybe(() => json()),
      });
      for (let i = 0; i < 150; i++) {
        const params: ThingParams = {
          ...fixed,
          label: str(),
          tags: maybe(() => arr(() => maybe(str), 6)),
          colors: maybe(() => arr(() => maybe(() => pick(labels)))),
          shape: maybe(randomShape),
          shapes: maybe(() => arr(() => maybe(randomShape))),
          attrs: maybe(() => Object.fromEntries(arr(() => [str(), maybe(str)] as const, 5))),
          dur: {
            months: Math.floor(random() * 2000) - 1000,
            days: Math.floor(random() * 2000) - 1000,
            microseconds: BigInt(Math.floor(random() * 2e12) - 1e12),
          },
          bigs: arr(() => maybe(() => BigInt.asIntN(64, BigInt(Math.floor(random() * 2 ** 53)) * 1031n))),
          blobs: arr(() => maybe(() => Uint8Array.from(arr(() => Math.floor(random() * 256), 8)))),
        };
        const id = await t.insertThing.fetchValue(c.db, params);
        const { j, ...row } = await t.thingWithJson.fetchOne(c.db, { id });
        const expected = { ...fixedRow, ...params };
        for (const col of ["label", "tags", "colors", "shape", "shapes", "attrs", "dur", "bigs", "blobs"] as const) {
          assert.deepEqual(row[col], expected[col], `case ${i}, ${col}: ${JSON.stringify(params[col], (_, v) => (typeof v === "bigint" ? `${v}n` : v))}`);
        }
        const oracle = j as Record<string, unknown>;
        for (const col of ["label", "tags", "colors", "shape", "shapes", "attrs"] as const) {
          assert.deepEqual(row[col], oracle[col], `case ${i}, ${col} vs to_jsonb`);
        }
      }
    });

    test("fetchStream yields every row, in batches, and releases on break", async () => {
      const before = await t.streamThings.fetchAll(c.db);
      await t.copyThings.execute(
        c.db,
        Array.from({ length: 1000 }, (_, i) => ({
          label: `s${i}`,
          tags: null,
          colors: null,
          shape: null,
          span: null,
          attrs: null,
          big: null,
          dur: null,
        })),
      );
      const seen: string[] = [];
      for await (const row of t.streamThings.fetchStream(c.db, {}, { batchSize: 7 })) seen.push(row.label);
      assert.equal(seen.length, before.length + 1000);
      assert.deepEqual(seen.slice(before.length, before.length + 3), ["s0", "s1", "s2"]);
      // Breaking out ends the cursor and gives the connection back: the
      // executor keeps working.
      let n = 0;
      for await (const _ of t.streamThings.fetchStream(c.db)) if (++n === 3) break;
      assert.equal(n, 3);
      assert.equal(typeof (await t.thingById.fetchOptional(c.db, { id: -1 })), "object");
      // An error raised while the server produces rows comes through.
      await assert.rejects(async () => {
        for await (const _ of t.labelsByColor.fetchStream(c.db, { color: "red" })) {
          // fine
        }
        throw new Error("done");
      }, /done/);
    });

    test("copyIn: tricky text, nested values, and all-or-nothing", async () => {
      const rows: CopyRow<typeof t.copyThings>[] = [
        {
          label: "tab\there\nnew\\line \\N",
          tags: ["\t", "\n", "\\N", null, ""],
          colors: ["back\\slash", null],
          shape,
          span: range(-5, 5),
          attrs: { "k\tey": "v\nal", n: null },
          big: -42n,
          dur: "1 day 02:00:00",
        },
        { label: "", tags: [], colors: [], shape: null, span: emptyRange, attrs: {}, big: 0, dur: null },
      ];
      const copied = await t.copyThings.execute(c.db, rows);
      assert.equal(copied, 2);
      const ids = (await t.streamThings.fetchAll(c.db)).slice(-2).map((r) => r.id);
      const back = await Promise.all(ids.map((id) => t.thingById.fetchOne(c.db, { id })));
      assert.equal(back[0]!.label, rows[0]!.label);
      assert.deepEqual(back[0]!.tags, rows[0]!.tags);
      assert.deepEqual(back[0]!.shape, shape);
      assert.deepEqual(back[0]!.attrs, rows[0]!.attrs);
      assert.equal(back[0]!.big, -42n);
      assert.deepEqual(back[0]!.dur, { months: 0, days: 1, microseconds: 7_200_000_000n });
      assert.deepEqual(back[1]!.span, emptyRange);
      assert.deepEqual(back[1]!.attrs, {});

      const count = (await t.streamThings.fetchAll(c.db)).length;
      // A row the server rejects: nothing of the COPY is kept.
      await assert.rejects(
        t.copyThings.execute(c.db, [{ ...rows[1]!, label: "ok" }, { ...rows[1]!, label: null as unknown as string }]),
      );
      // A source that fails midway: likewise.
      async function* failing() {
        yield { ...rows[1]!, label: "first" };
        throw new Error("source failed");
      }
      await assert.rejects(t.copyThings.execute(c.db, failing()), /source failed/);
      assert.equal((await t.streamThings.fetchAll(c.db)).length, count);
      // The connection is usable afterwards.
      await assert.rejects(t.thingById.fetchOne(c.db, { id: -1 }), NoRowsError);
    });
  });
}
