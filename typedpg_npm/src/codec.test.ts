import assert from "node:assert/strict";
import { test } from "node:test";

import { type Range, decode, encode } from "./codec.js";

test("scalars", () => {
  assert.equal(decode("bool", "t"), true);
  assert.equal(decode("bool", "f"), false);
  assert.equal(decode("number", "-12.5"), -12.5);
  assert.equal(decode("number", "-Infinity"), -Infinity);
  assert.ok(Number.isNaN(decode("number", "NaN")));
  assert.equal(decode("bigint", "9223372036854775807"), 9223372036854775807n);
  assert.deepEqual(decode("json", '{"a":[1,null]}'), { a: [1, null] });
  assert.deepEqual(decode("bytea", "\\x00ff10"), Buffer.from([0, 255, 16]));
  assert.ok(Buffer.isBuffer(decode("bytea", "\\x00ff10")));
  // `bytea_output = escape`.
  assert.ok(Buffer.isBuffer(decode("bytea", "a\\\\\\001")));
  assert.deepEqual(decode("bytea", "a\\\\\\001"), Buffer.from([0x61, 0x5c, 1]));
});

test("timestamptz", () => {
  const t = (s: string) => (decode("timestamptz", s) as Temporal.Instant).toString();
  assert.ok(decode("timestamptz", "2024-01-02 03:04:05+00") instanceof Temporal.Instant);
  // Every digit PG has: microseconds.
  assert.equal(t("2024-01-02 03:04:05.123456+00"), "2024-01-02T03:04:05.123456Z");
  assert.equal(t("2024-01-02 03:04:05-03"), "2024-01-02T06:04:05Z");
  assert.equal(t("2024-01-02 03:04:05.5+05:30"), "2024-01-01T21:34:05.5Z");
  assert.equal(t("1890-01-01 00:00:00+00:53:28"), "1889-12-31T23:06:32Z");
  assert.equal(t("0044-03-15 12:00:00+00 BC"), "-000043-03-15T12:00:00Z");
  assert.equal(t("0099-01-01 00:00:00+00"), "0099-01-01T00:00:00Z");
  assert.equal(t("1900-03-01 00:00:00+00"), "1900-03-01T00:00:00Z");
  assert.equal(t("2000-02-29 23:59:59.999999+00"), "2000-02-29T23:59:59.999999Z");
  assert.equal(t("10000-01-01 00:00:00+00"), "+010000-01-01T00:00:00Z");
  assert.equal(t("infinity"), "+275760-09-13T00:00:00Z");
  assert.equal(t("-infinity"), "-271821-04-20T00:00:00Z");
  // PG's range goes further than an Instant's.
  assert.throws(() => decode("timestamptz", "294276-12-31 23:59:59+00"), /out of a Temporal.Instant's range/);
  assert.throws(() => decode("timestamptz", "01/02/2024 03:04:05 UTC"), /ISO DateStyle/);
});

test("timestamp, date, time", () => {
  const ts = (s: string) => (decode("timestamp", s) as Temporal.PlainDateTime).toString();
  assert.ok(decode("timestamp", "2024-01-02 03:04:05") instanceof Temporal.PlainDateTime);
  assert.equal(ts("2024-01-02 03:04:05.123456"), "2024-01-02T03:04:05.123456");
  assert.equal(ts("0044-03-15 12:00:00 BC"), "-000043-03-15T12:00:00");
  assert.equal(ts("infinity"), "+275760-09-13T00:00:00");
  assert.equal(ts("-infinity"), "-271821-04-20T00:00:00");
  assert.throws(() => decode("timestamp", "2024-01-02 03:04:05+00"), /ISO DateStyle/);

  const d = (s: string) => (decode("date", s) as Temporal.PlainDate).toString();
  assert.ok(decode("date", "2024-02-29") instanceof Temporal.PlainDate);
  assert.equal(d("2024-02-29"), "2024-02-29");
  assert.equal(d("0001-01-01 BC"), "0000-01-01");
  assert.equal(d("12345-06-07"), "+012345-06-07");
  assert.equal(d("infinity"), "+275760-09-13");
  assert.equal(d("-infinity"), "-271821-04-20");
  assert.throws(() => decode("date", "5874897-12-31"), /out of a Temporal.PlainDate's range/);

  const t = (s: string) => (decode("time", s) as Temporal.PlainTime).toString();
  assert.ok(decode("time", "04:05:06") instanceof Temporal.PlainTime);
  assert.equal(t("04:05:06"), "04:05:06");
  assert.equal(t("23:59:59.999999"), "23:59:59.999999");
  assert.throws(() => decode("time", "24:00:00"), /out of a Temporal.PlainTime's range/);
});

test("interval", () => {
  const i = (s: string) => (decode("interval", s) as Temporal.Duration).toString();
  assert.ok(decode("interval", "1 day") instanceof Temporal.Duration);
  assert.equal(i("1 year 2 mons 3 days 04:05:06.789"), "P1Y2M3DT4H5M6.789S");
  assert.equal(i("-1 years -2 mons -3 days -04:05:06.000001"), "-P1Y2M3DT4H5M6.000001S");
  assert.equal(i("00:00:00"), "PT0S");
  assert.equal(i("2562047788:00:54.775807"), "PT2562047788H54.775807S");
  assert.equal(i("P1Y2M3DT4H5M6.5S"), "P1Y2M3DT4H5M6.5S");
  // A Duration's fields share one sign.
  assert.throws(() => decode("interval", "1 mon -1 days"), /mixes signs.*justify_interval/);
  assert.throws(() => decode("interval", "1 day -01:00:00"), /mixes signs/);
  assert.throws(() => decode("interval", "infinity"), /infinite interval/);
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

test("a box array separates its elements with semicolons", () => {
  // `box`'s typdelim is `;`: its own text holds commas.
  assert.deepEqual(decode(["array", "box"], "{(1,1),(0,0);(3,3),(2,2);NULL}"), [
    "(1,1),(0,0)",
    "(3,3),(2,2)",
    null,
  ]);
  assert.equal(encode(["array", "box"], ["(1,1),(0,0)", null, "(3,3),(2,2)"]), '{"(1,1),(0,0)";NULL;"(3,3),(2,2)"}');
  assert.equal(decode("box", "(1,1),(0,0)"), "(1,1),(0,0)");
});

test("int2vector and oidvector print their elements space-separated", () => {
  assert.deepEqual(decode(["array", "number"], "1 2 -3"), [1, 2, -3]);
  assert.deepEqual(decode(["array", "number"], ""), []);
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
  assert.equal(encode("bool", true), "t");
  // A boolean, not a truthy value: `encode("bool", "false")` would store
  // `t`, silently.
  assert.throws(() => encode("bool", "false"), TypeError);
  assert.throws(() => encode("bool", 0), TypeError);
  assert.throws(() => encode("bool", {}), TypeError);
  // NULL is the `Option<bool>` of Rust, not a boolean.
  assert.equal(encode("bool", null), null);
  assert.equal(encode("bool", undefined), null);
  // The nested paths encoded it as `t` too.
  assert.throws(() => encode(["array", "bool"], ["false"]), TypeError);
  assert.throws(() => encode(["record", [["f", "bool"]]], { f: "false" }), TypeError);
  assert.equal(encode("bigint", 12n), "12");
  assert.equal(encode("json", { a: "x" }), '{"a":"x"}');
  assert.equal(encode("json", "x"), '"x"');
  assert.equal(encode("bytea", new Uint8Array([1, 255])), "\\x01ff");
  assert.throws(() => encode("bytea", "00ff"), /a bytea parameter must be a Buffer or a Uint8Array, not a string/);
  assert.equal(encode(["array", "text"], ["a", null, 'q"\\']), '{"a",NULL,"q\\"\\\\"}');
  assert.equal(encode("text", null), null);
});

test("a float's negative zero is encoded as one", () => {
  assert.equal(encode("number", -0), "-0");
  assert.equal(encode("number", 0), "0");
  assert.equal(encode(["array", "number"], [-0, 1.5]), '{"-0","1.5"}');
  assert.ok(Object.is(decode("number", encode("number", -0)!), -0));
});

test("encode dates and times", () => {
  const instant = (s: string) => Temporal.Instant.from(s);
  assert.equal(encode("timestamptz", instant("1970-01-01T00:00:00Z")), "1970-01-01 00:00:00.000000000Z");
  // Before year 1: PG's `BC`, not Temporal's `-000043`; after 9999, no `+`.
  assert.equal(encode("timestamptz", instant("0044-03-15T12:00:00Z")), "0044-03-15 12:00:00.000000000Z");
  assert.equal(encode("timestamptz", instant("-000043-03-15T12:00:00Z")), "0044-03-15 12:00:00.000000000Z BC");
  assert.equal(encode("timestamptz", instant("+010000-01-01T00:00:00Z")), "10000-01-01 00:00:00.000000000Z");
  assert.equal(encode("timestamptz", instant("2024-01-02T03:04:05.123456789Z")), "2024-01-02 03:04:05.123456789Z");
  // A ZonedDateTime is its instant.
  assert.equal(
    encode("timestamptz", Temporal.ZonedDateTime.from("2024-01-02T00:30:00-03:00[America/Sao_Paulo]")),
    "2024-01-02 03:30:00.000000000Z",
  );
  // A Date is an instant too, BC included.
  assert.equal(encode("timestamptz", new Date(0)), "1970-01-01 00:00:00.000000000Z");
  assert.equal(encode("timestamptz", new Date("2024-01-02T03:04:05.678Z")), "2024-01-02 03:04:05.678000000Z");
  assert.equal(encode("timestamptz", new Date(-62200000000000)), "0003-12-17 14:13:20.000000000Z BC");
  assert.throws(() => encode("timestamptz", new Date(NaN)), /invalid Date/);
  // Not for the zoneless types: which day or time a Date is depends on a zone.
  assert.throws(() => encode("timestamp", new Date(0)), /must be a Temporal.PlainDateTime, not a Date/);
  assert.throws(() => encode("date", new Date(0)), /must be a Temporal.PlainDate, not a Date/);
  assert.throws(() => encode("timestamptz", "2024-01-01"), /not string/);

  assert.equal(
    encode("timestamp", Temporal.PlainDateTime.from("2024-02-29T12:34:56.5")),
    "2024-02-29 12:34:56.500000000",
  );
  assert.equal(encode("timestamp", Temporal.PlainDateTime.from("0000-12-31T00:00")), "0001-12-31 00:00:00.000000000 BC");
  assert.throws(() => encode("timestamp", Temporal.Instant.from("2024-01-01T00:00Z")), /not an Instant/);

  assert.equal(encode("date", Temporal.PlainDate.from("2024-02-29")), "2024-02-29");
  assert.equal(encode("date", Temporal.PlainDate.from("-000043-03-15")), "0044-03-15 BC");
  // Another calendar's date is the same day.
  assert.equal(encode("date", Temporal.PlainDate.from("2024-02-29").withCalendar("hebrew")), "2024-02-29");
  assert.throws(() => encode("date", Temporal.PlainDateTime.from("2024-02-29T00:00")), /must be a Temporal.PlainDate/);

  assert.equal(encode("time", Temporal.PlainTime.from("04:05:06.000001")), "04:05:06.000001000");
  assert.throws(() => encode("time", "04:05:06"), /must be a Temporal.PlainTime/);

  const dur = (s: string) => encode("interval", Temporal.Duration.from(s));
  assert.equal(dur("P1Y2M3W4DT5H6M7.008009S"), "14 mons 25 days 18367008009 microseconds");
  assert.equal(dur("-P1Y2M3W4DT5H6M7.008009S"), "-14 mons -25 days -18367008009 microseconds");
  assert.equal(dur("PT0.000000001S"), "0 mons 0 days 0.001 microseconds");
  assert.equal(dur("-PT1.0000005S"), "0 mons 0 days -1000000.500 microseconds");
  assert.throws(() => encode("interval", "1 day"), /must be a Temporal.Duration/);
  // Arrays and ranges of them.
  assert.equal(encode(["array", "date"], [Temporal.PlainDate.from("2024-01-01"), null]), '{"2024-01-01",NULL}');
  // Compared as strings: `deepEqual` sees no fields on a Temporal value.
  const r = decode(["range", "date"], "[2024-01-01,infinity)") as Range<Temporal.PlainDate>;
  assert.ok(!r.empty);
  assert.equal(r.lower?.value.toString(), "2024-01-01");
  assert.equal(r.upper?.value.toString(), "+275760-09-13");
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
