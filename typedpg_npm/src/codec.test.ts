import assert from "node:assert/strict";
import { test } from "node:test";

import { decode, encode } from "./codec.js";

test("scalars", () => {
  assert.equal(decode("bool", "t"), true);
  assert.equal(decode("bool", "f"), false);
  assert.equal(decode("number", "-12.5"), -12.5);
  assert.equal(decode("number", "-Infinity"), -Infinity);
  assert.ok(Number.isNaN(decode("number", "NaN")));
  assert.equal(decode("bigint", "9223372036854775807"), 9223372036854775807n);
  assert.deepEqual(decode("json", '{"a":[1,null]}'), { a: [1, null] });
  assert.deepEqual(decode("bytea", "\\x00ff10"), new Uint8Array([0, 255, 16]));
});

test("timestamptz", () => {
  const t = (s: string) => (decode("timestamptz", s) as Date).toISOString();
  assert.equal(t("2024-01-02 03:04:05.123456+00"), "2024-01-02T03:04:05.123Z");
  assert.equal(t("2024-01-02 03:04:05-03"), "2024-01-02T06:04:05.000Z");
  assert.equal(t("2024-01-02 03:04:05.5+05:30"), "2024-01-01T21:34:05.500Z");
  assert.equal(t("1890-01-01 00:00:00+00:53:28"), "1889-12-31T23:06:32.000Z");
  assert.equal(t("0044-03-15 12:00:00+00 BC"), "-000043-03-15T12:00:00.000Z");
  assert.equal(t("0099-01-01 00:00:00+00"), "0099-01-01T00:00:00.000Z");
});

test("arrays", () => {
  assert.deepEqual(decode(["array", "number"], "{1,2,NULL}"), [1, 2, null]);
  assert.deepEqual(decode(["array", "text"], "{}"), []);
  assert.deepEqual(decode(["array", "text"], '{a,"b c","q\\"uo\\\\te","NULL",null}'), [
    "a",
    "b c",
    'q"uo\\te',
    "NULL",
    null,
  ]);
  assert.deepEqual(decode(["array", "number"], "[0:1]={5,6}"), [5, 6]);
  assert.throws(() => decode(["array", "number"], "{{1},{2}}"), /multidimensional/);
});

test("records", () => {
  const codec = ["record", [["a", "number"], ["b", "text"], ["c", "text"], ["d", ["array", "number"]]]] as const;
  assert.deepEqual(decode(codec, '(1,"x ""y"" \\\\z",,"{1,2}")'), {
    a: 1,
    b: 'x "y" \\z',
    c: null,
    d: [1, 2],
  });
  assert.deepEqual(decode(["record", [["a", "text"]]], '("")'), { a: "" });
});

test("encode", () => {
  assert.equal(encode("bool", false), "f");
  assert.equal(encode("bigint", 12n), "12");
  assert.equal(encode("json", { a: "x" }), '{"a":"x"}');
  assert.equal(encode("json", "x"), '"x"');
  assert.equal(encode("timestamptz", new Date(0)), "1970-01-01 00:00:00.000Z");
  // Before year 1: PG's `BC`, not toISOString's `-000043`.
  assert.equal(encode("timestamptz", new Date("0044-03-15T12:00:00Z")), "0044-03-15 12:00:00.000Z");
  assert.equal(encode("timestamptz", new Date(-62200000000000)), "0003-12-17 14:13:20.000Z BC");
  assert.throws(() => encode("timestamptz", new Date(NaN)), /invalid Date/);
  assert.equal(encode("bytea", new Uint8Array([1, 255])), "\\x01ff");
  assert.equal(encode(["array", "text"], ["a", null, 'q"\\']), '{"a",NULL,"q\\"\\\\"}');
  assert.equal(encode("text", null), null);
});

test("json keys of one escaped character survive V8's key bug", () => {
  // Node 24/26: after a `"\\"` key, `JSON.parse` reads `"\t"` as `"\\"`.
  JSON.parse(String.raw`{"\\": 1}`);
  assert.deepEqual(Object.keys(decode("json", String.raw`{"\t": 1, "\n": 2, "\/": 3, "\u0041": 4, "\"": 5, "\\": 6}`) as object), [
    "\t",
    "\n",
    "/",
    "A",
    '"',
    "\\",
  ]);
  // Nested, in arrays, and next to strings that only look like such keys.
  assert.deepEqual(decode("json", String.raw`[{"\t": {"\n": ["\t", {"\r": null}]}, "x\"\t\":": "\"\t\":"}]`), [
    { "\t": { "\n": ["\t", { "\r": null }] }, 'x"\t":': '"\t":' },
  ]);
  // No such key: the plain parse.
  assert.deepEqual(decode("json", '{"a": "\\t"}'), { a: "\t" });
});
