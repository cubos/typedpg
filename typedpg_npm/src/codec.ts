// Conversion between PostgreSQL's text format and JS values. Every column is
// read as text (the adapters turn the drivers' own parsing off), so a value
// is what the generated module's type for it promises, whatever type parsers
// the application set up on its driver.
//
// Dates and times are Temporal's (Node 26+): the reference below keeps
// TypeScript's Temporal declarations in the emitted declarations, so a
// project using the generated modules sees them without a `lib` setting.

/// <reference lib="esnext.temporal" preserve="true" />

/** How a value is converted; the generator writes one per column and parameter. */
export type Codec =
  | "text"
  // `box`: text, but its arrays separate elements with `;` (its typdelim).
  | "box"
  | "bool"
  | "number"
  | "bigint"
  | "json"
  | "timestamptz"
  | "timestamp"
  | "date"
  | "time"
  | "bytea"
  | "interval"
  | "vector"
  | "hstore"
  | "void"
  | readonly ["array", Codec]
  | readonly ["range", Codec]
  | readonly ["multirange", Codec]
  | readonly ["record", readonly (readonly [string, Codec])[]];

/** A value of a PostgreSQL range type. */
export type Range<T> =
  | { readonly empty: true }
  | {
      readonly empty: false;
      /** `null`: unbounded below. */
      readonly lower: RangeBound<T> | null;
      /** `null`: unbounded above. */
      readonly upper: RangeBound<T> | null;
    };

export interface RangeBound<T> {
  readonly value: T;
  readonly inclusive: boolean;
}

/** The empty range. */
export const emptyRange: Range<never> = Object.freeze({ empty: true as const });

/**
 * The range between `lower` and `upper` (`null`: unbounded), with `bounds`
 * as PostgreSQL writes them: `"[)"` (the default, as PG's constructors),
 * `"[]"`, `"(]"` or `"()"`.
 *
 * PostgreSQL canonicalizes a discrete range (`int4range`, `int8range`,
 * `daterange`) to `[)`, so `range(1, 5, "[]")` reads back as `[1, 6)`.
 */
export function range<T>(lower: T | null, upper: T | null, bounds: "[)" | "[]" | "(]" | "()" = "[)"): Range<T> {
  return {
    empty: false,
    lower: lower === null ? null : { value: lower, inclusive: bounds[0] === "[" },
    upper: upper === null ? null : { value: upper, inclusive: bounds[1] === "]" },
  };
}

/** `obj[key] = value`, as an own property even for `__proto__`. */
export function setOwn(obj: Record<string, unknown>, key: string, value: unknown): void {
  if (key === "__proto__") {
    Object.defineProperty(obj, key, { value, writable: true, enumerable: true, configurable: true });
  } else {
    obj[key] = value;
  }
}

export class DecodeError extends Error {
  override name = "DecodeError";
}

/** A column's text value as the JS value its codec stands for. */
export function decode(codec: Codec, text: string): unknown {
  if (typeof codec !== "string") {
    switch (codec[0]) {
      case "array":
        return parseArray(text, codec[1]);
      case "range":
        return parseRange(text, codec[1]);
      case "multirange":
        return parseMultirange(text, codec[1]);
      case "record":
        return parseRecord(text, codec[1]);
    }
  }
  switch (codec) {
    case "text":
    case "box":
      return text;
    case "bool":
      return text === "t";
    case "number":
      // `NaN`, `Infinity` and `-Infinity` are what PG prints for those.
      return Number(text);
    case "bigint":
      return BigInt(text);
    case "json":
      return parseJson(text);
    case "timestamptz":
      return parseTimestamptz(text);
    case "timestamp":
      return parseTimestamp(text);
    case "date":
      return parseDate(text);
    case "time":
      return parseTime(text);
    case "bytea":
      return parseBytea(text);
    case "interval":
      return parseInterval(text);
    case "vector":
      return text.length <= 2 ? [] : text.slice(1, -1).split(",").map(Number);
    case "hstore":
      return parseHstore(text);
    case "void":
      return undefined;
  }
}

/** A parameter's value as the text PG reads it from. */
export function encode(codec: Codec, value: unknown): string | null {
  if (value === null || value === undefined) return null;
  if (typeof codec !== "string") {
    switch (codec[0]) {
      case "array":
        return encodeArray(codec[1], value as readonly unknown[]);
      case "range":
        return encodeRange(codec[1], value as Range<unknown>);
      case "multirange":
        return "{" + (value as readonly Range<unknown>[]).map((r) => encodeRange(codec[1], r)).join(",") + "}";
      case "record":
        return encodeRecord(codec[1], value as Record<string, unknown>);
    }
  }
  switch (codec) {
    case "bool":
      // A boolean, not a truthy value: `encode("bool", "false")` would
      // store `t`, silently.
      if (typeof value !== "boolean") {
        throw new TypeError(`typedpg: a bool parameter must be a boolean, not ${typeof value}`);
      }
      return value ? "t" : "f";
    case "json":
      return JSON.stringify(value);
    case "timestamptz":
      return formatTimestamptz(value);
    case "timestamp":
      return formatTimestamp(value);
    case "date":
      return formatDate(value);
    case "time":
      return formatTime(value);
    case "bytea":
      // Bytes, not text: `Buffer.from` would take a string's UTF-8 bytes,
      // storing a hex digest's text, silently.
      if (typeof value === "string") {
        throw new TypeError("typedpg: a bytea parameter must be a Buffer or a Uint8Array, not a string");
      }
      return "\\x" + Buffer.from(value as Uint8Array).toString("hex");
    case "interval":
      return formatInterval(value);
    case "number":
      // `String(-0)` is `0`: a float's negative zero would be lost.
      return Object.is(value, -0) ? "-0" : String(value);
    case "vector":
      return "[" + (value as readonly number[]).join(",") + "]";
    case "hstore":
      return Object.entries(value as Record<string, string | null>)
        .map(([k, v]) => quoteHstore(k) + "=>" + (v === null ? "NULL" : quoteHstore(v)))
        .join(",");
    default:
      return String(value);
  }
}

/** What an array of `codec` separates its elements with (`pg_type.typdelim`). */
function delimiter(codec: Codec): string {
  return codec === "box" ? ";" : ",";
}

/** `"…"` with `"` and `\` escaped, as array, record and range literals read it. */
function quote(text: string): string {
  return '"' + text.replace(/[\\"]/g, "\\$&") + '"';
}

const quoteHstore = quote;

function encodeArray(codec: Codec, items: readonly unknown[]): string {
  const parts = items.map((item) => {
    const text = encode(codec, item);
    return text === null ? "NULL" : quote(text);
  });
  return "{" + parts.join(delimiter(codec)) + "}";
}

function encodeRecord(fields: readonly (readonly [string, Codec])[], value: Record<string, unknown>): string {
  // An empty unquoted field is NULL; anything else is quoted.
  const parts = fields.map(([name, codec]) => {
    const text = encode(codec, value[name]);
    return text === null ? "" : quote(text);
  });
  return "(" + parts.join(",") + ")";
}

function encodeRange(codec: Codec, r: Range<unknown>): string {
  if (r.empty) return "empty";
  const bound = (b: RangeBound<unknown> | null) => {
    const text = b === null ? null : encode(codec, b.value);
    return text === null ? "" : quote(text);
  };
  return (
    (r.lower?.inclusive ? "[" : "(") + bound(r.lower) + "," + bound(r.upper) + (r.upper?.inclusive ? "]" : ")")
  );
}

/**
 * Read one element of an array / record / range literal starting at `i`:
 * quoted (backslash escapes, and `""` for a quote where `doubled`) or bare up
 * to one of `stops`. Returns the text, whether it was quoted, and the index
 * past it.
 */
function readItem(text: string, i: number, stops: string, doubled: boolean): [string, boolean, number] {
  let out = "";
  let quoted = false;
  while (i < text.length && !stops.includes(text[i]!)) {
    const c = text[i]!;
    if (c === '"') {
      quoted = true;
      i++;
      for (;;) {
        if (i >= text.length) throw new DecodeError(`typedpg: unterminated quote in ${text}`);
        const q = text[i]!;
        if (q === "\\") {
          out += text[i + 1];
          i += 2;
        } else if (q === '"') {
          if (doubled && text[i + 1] === '"') {
            out += '"';
            i += 2;
          } else {
            i++;
            break;
          }
        } else {
          out += q;
          i++;
        }
      }
    } else if (c === "\\") {
      out += text[i + 1];
      i += 2;
    } else {
      out += c;
      i++;
    }
  }
  return [out, quoted, i];
}

/**
 * `{a,"b c",NULL}` (with an optional `[1:3]=` bounds decoration, and `;`
 * between the elements of a `box[]`) as an array.
 * A multidimensional array is an error, as on the Rust side: its type is the
 * one-dimensional one's.
 */
function parseArray(text: string, codec: Codec): unknown[] {
  const delim = delimiter(codec);
  // `int2vector` and `oidvector` are arrays of `int2` / `oid` to the
  // analysis, but print their elements space-separated, without braces
  // (`1 2 3`, the empty one as nothing); no element is NULL or quoted.
  if (text[0] !== "{" && text[0] !== "[") {
    return text.split(" ").filter((item) => item !== "").map((item) => decode(codec, item));
  }
  let i = 0;
  if (text[0] === "[") {
    i = text.indexOf("=") + 1;
  }
  if (text[i] !== "{") throw new DecodeError(`typedpg: malformed array: ${text}`);
  i++;
  const out: unknown[] = [];
  if (text[i] === "}") return out;
  for (;;) {
    if (text[i] === "{") {
      throw new DecodeError("typedpg: multidimensional arrays are not supported");
    }
    const [item, quoted, next] = readItem(text, i, delim + "}", false);
    i = next;
    out.push(!quoted && item.toUpperCase() === "NULL" ? null : decode(codec, item));
    if (text[i] === "}") return out;
    if (text[i] !== delim) throw new DecodeError(`typedpg: malformed array: ${text}`);
    i++;
  }
}

/** `(1,"a b",)` as an object: an empty unquoted field is NULL. */
function parseRecord(text: string, fields: readonly (readonly [string, Codec])[]): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  let i = 1; // "("
  for (const [name, codec] of fields) {
    const [item, quoted, next] = readItem(text, i, ",)", true);
    i = next + 1; // "," or ")"
    setOwn(out, name, item === "" && !quoted ? null : decode(codec, item));
  }
  return out;
}

/** `empty`, or `[a,b)` with a missing bound unbounded. */
function parseRange(text: string, codec: Codec): Range<unknown> {
  if (text === "empty") return emptyRange;
  const lowerInclusive = text[0] === "[";
  const [lower, lowerQuoted, comma] = readItem(text, 1, ",", true);
  const [upper, upperQuoted, close] = readItem(text, comma + 1, ")]", true);
  const bound = (v: string, quoted: boolean, inclusive: boolean) =>
    v === "" && !quoted ? null : { value: decode(codec, v), inclusive };
  return {
    empty: false,
    lower: bound(lower, lowerQuoted, lowerInclusive),
    upper: bound(upper, upperQuoted, text[close] === "]"),
  };
}

/** `{[1,3),[5,7)}` as its ranges. */
function parseMultirange(text: string, codec: Codec): Range<unknown>[] {
  const out: Range<unknown>[] = [];
  let i = 1;
  while (i < text.length - 1) {
    if (text[i] === ",") i++;
    if (text.startsWith("empty", i)) {
      out.push(emptyRange);
      i += 5;
      continue;
    }
    // Find this range's closing bracket, outside quotes.
    let j = i + 1;
    let inQuote = false;
    for (; j < text.length; j++) {
      const c = text[j];
      if (c === "\\") j++;
      else if (c === '"') inQuote = !inQuote;
      else if (!inQuote && (c === ")" || c === "]")) break;
    }
    out.push(parseRange(text.slice(i, j + 1), codec));
    i = j + 1;
  }
  return out;
}

/** `"a"=>"1", "b"=>NULL` as an object. */
function parseHstore(text: string): Record<string, string | null> {
  const out: Record<string, string | null> = {};
  let i = 0;
  while (i < text.length) {
    while (text[i] === " " || text[i] === ",") i++;
    if (i >= text.length) break;
    const [key, , afterKey] = readItem(text, i, "=", false);
    i = afterKey + 2; // "=>"
    const [value, quoted, next] = readItem(text, i, ",", false);
    setOwn(out, key, !quoted && value === "NULL" ? null : value);
    i = next;
  }
  return out;
}

/**
 * `JSON.parse`, working around a V8 bug (Node 24 and 26; Node 22 is fine):
 * once any `JSON.parse` in the process has read a key that is just an
 * escaped backslash (`{"\\": …}`), every later key of one escaped
 * character (`"\t"`, `"\n"`, `"\u0009"`…) comes back as a backslash.
 * Such keys get a `\u0000` prefix for the parse, then lose it: PostgreSQL's
 * JSON can't hold NUL, so the prefix is never part of a real key.
 */
function parseJson(text: string): unknown {
  if (!ONE_ESCAPE_KEY.test(text)) return JSON.parse(text);
  const marked = text.replace(JSON_STRING, (token, offset: number) =>
    /^"\\(?:u[0-9a-fA-F]{4}|[^u])"$/.test(token) && /^\s*:/.test(text.slice(offset + token.length, offset + token.length + 64))
      ? '"\\u0000' + token.slice(1)
      : token,
  );
  return unmark(JSON.parse(marked));
}

/** A key of one escaped character, maybe: the cheap test before the exact one. */
const ONE_ESCAPE_KEY = /"\\(?:u[0-9a-fA-F]{4}|[^u])"\s*:/;
/** A JSON string token. */
const JSON_STRING = /"(?:[^"\\]|\\.)*"/g;

function unmark(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(unmark);
  if (value === null || typeof value !== "object") return value;
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(value)) setOwn(out, k.startsWith("\0") ? k.slice(1) : k, unmark(v));
  return out;
}

/** The hex format (`\x0102`, the default `bytea_output`) or the escape one. */
function parseBytea(text: string): Buffer {
  if (text.startsWith("\\x")) {
    return Buffer.from(text.slice(2), "hex");
  }
  // `bytea_output = escape`: printable ASCII as is, `\\` and `\ooo` octal.
  const out: number[] = [];
  for (let i = 0; i < text.length; ) {
    if (text[i] === "\\") {
      if (text[i + 1] === "\\") {
        out.push(0x5c);
        i += 2;
      } else {
        out.push(parseInt(text.slice(i + 1, i + 4), 8));
        i += 4;
      }
    } else {
      out.push(text.charCodeAt(i));
      i++;
    }
  }
  return Buffer.from(out);
}

/**
 * An interval in the `postgres` IntervalStyle (the default: `1 year 2 mons
 * -3 days 04:05:06.5`) or `iso_8601` (`P1Y2M-3DT4H5M6.5S`).
 */
function parseInterval(text: string): Temporal.Duration {
  let months = 0;
  let days = 0;
  let micros = 0n;
  const seconds = (s: string) => {
    // Exact: `-04:05:06.123456` as microseconds, without float rounding.
    const neg = s.startsWith("-");
    const [whole, frac = ""] = s.replace(/^[+-]/, "").split(".");
    const v = BigInt(whole!) * 1_000_000n + BigInt((frac + "000000").slice(0, 6));
    return neg ? -v : v;
  };
  if (text.startsWith("P")) {
    const m = /^P(?:(-?\d+)Y)?(?:(-?\d+)M)?(?:(-?\d+)W)?(?:(-?\d+)D)?(?:T(?:(-?\d+)H)?(?:(-?\d+)M)?(?:(-?[\d.]+)S)?)?$/.exec(
      text,
    );
    if (!m) throw new DecodeError(`typedpg: unreadable interval ${JSON.stringify(text)}`);
    months = Number(m[1] ?? 0) * 12 + Number(m[2] ?? 0);
    days = Number(m[3] ?? 0) * 7 + Number(m[4] ?? 0);
    micros =
      BigInt(m[5] ?? 0) * 3_600_000_000n + BigInt(m[6] ?? 0) * 60_000_000n + (m[7] ? seconds(m[7]) : 0n);
    return duration(text, months, days, micros);
  }
  const re = /([+-]?\d+) (years?|mons?|days?)|([+-]?)(\d+):(\d\d):(\d\d(?:\.\d+)?)/g;
  let matched = 0;
  for (const m of text.matchAll(re)) {
    matched += m[0].length + 1;
    if (m[1] !== undefined) {
      const n = Number(m[1]);
      if (m[2]!.startsWith("year")) months += n * 12;
      else if (m[2]!.startsWith("mon")) months += n;
      else days += n;
    } else {
      const v = BigInt(m[4]!) * 3_600_000_000n + BigInt(m[5]!) * 60_000_000n + seconds(m[6]!);
      micros += m[3] === "-" ? -v : v;
    }
  }
  if (text === "infinity" || text === "-infinity") {
    throw new DecodeError(`typedpg: a Temporal.Duration can't be an infinite interval (${text})`);
  }
  if (matched < text.length) {
    throw new DecodeError(
      `typedpg: unreadable interval ${JSON.stringify(text)}: typedpg reads the postgres and iso_8601 ` +
        `IntervalStyles (SET IntervalStyle = 'postgres')`,
    );
  }
  return duration(text, months, days, micros);
}

/**
 * An interval's fields as a Duration: years and months from its months,
 * hours down to microseconds from its time, as PostgreSQL prints them. A
 * Duration's fields share one sign, so an interval mixing signs (`1 mon
 * -1 days`, which only a date resolves) has none.
 */
function duration(text: string, months: number, days: number, micros: bigint): Temporal.Duration {
  const signs = [Math.sign(months), Math.sign(days), micros < 0n ? -1 : micros > 0n ? 1 : 0];
  if (signs.includes(1) && signs.includes(-1)) {
    throw new DecodeError(
      `typedpg: the interval ${JSON.stringify(text)} mixes signs, which a Temporal.Duration can't: ` +
        `justify_interval() makes its fields agree`,
    );
  }
  const sign = micros < 0n ? -1 : 1;
  let rest = micros < 0n ? -micros : micros;
  const take = (unit: bigint) => {
    const n = rest / unit;
    rest %= unit;
    return sign * Number(n) + 0;
  };
  const hours = take(3_600_000_000n);
  const minutes = take(60_000_000n);
  const seconds = take(1_000_000n);
  const milliseconds = take(1_000n);
  const microseconds = take(1n);
  // `+ 0`: no `-0` fields.
  return new Temporal.Duration(
    Math.trunc(months / 12) + 0,
    (months % 12) + 0,
    0,
    days,
    hours,
    minutes,
    seconds,
    milliseconds,
    microseconds,
  );
}

/**
 * A Duration as PostgreSQL reads it: its calendar units as months and
 * days (a week is 7 days, a year 12 months, as PG counts them), its time
 * as microseconds, a nanosecond remainder as their fraction (which PG
 * rounds).
 */
function formatInterval(value: unknown): string {
  if (!(value instanceof Temporal.Duration)) {
    throw new TypeError(`typedpg: an interval parameter must be a Temporal.Duration, not ${describe(value)}`);
  }
  const d = value;
  const ns =
    BigInt(d.hours) * 3_600_000_000_000n +
    BigInt(d.minutes) * 60_000_000_000n +
    BigInt(d.seconds) * 1_000_000_000n +
    BigInt(d.milliseconds) * 1_000_000n +
    BigInt(d.microseconds) * 1_000n +
    BigInt(d.nanoseconds);
  const abs = ns < 0n ? -ns : ns;
  const fraction = abs % 1_000n === 0n ? "" : "." + String(abs % 1_000n).padStart(3, "0");
  const micros = (ns < 0n ? "-" : "") + String(abs / 1_000n) + fraction;
  return `${d.years * 12 + d.months} mons ${d.weeks * 7 + d.days} days ${micros} microseconds`;
}

// ── Dates and times, in PostgreSQL's ISO DateStyle (the default) ──
//
// PG writes years before 1 as `0044 BC` and after 9999 with more digits;
// Temporal's own strings write them `-000043` and `+010000`, which PG
// rejects, so both ways are by hand. An `infinity` reads as the type's
// latest value and `-infinity` as its earliest, the bounds a `Date` shares
// (±8.64e15 ms from 1970): a Temporal value can't be infinite. A parameter
// can't either, which the analysis takes into account.

const DATE = String.raw`(\d{4,})-(\d\d)-(\d\d)`;
const TIME = String.raw`(\d\d):(\d\d):(\d\d)(?:\.(\d{1,9}))?`;
const OFFSET = String.raw`([+-])(\d\d)(?::(\d\d))?(?::(\d\d))?`;
const DATE_RE = new RegExp(`^${DATE}( BC)?$`);
const TIMESTAMP_RE = new RegExp(`^${DATE} ${TIME}( BC)?$`);
// The offset can carry seconds (`+00:53:28`, a historical zone).
const TIMESTAMPTZ_RE = new RegExp(`^${DATE} ${TIME}${OFFSET}( BC)?$`);
const TIME_RE = new RegExp(`^${TIME}$`);

function unreadable(type: string, text: string): DecodeError {
  return new DecodeError(
    `typedpg: unreadable ${type} ${JSON.stringify(text)}: typedpg reads PostgreSQL's ISO ` +
      `DateStyle, the default; this session uses another (SET DateStyle = 'ISO')`,
  );
}

/** Builds a Temporal value, reporting one out of its range as a DecodeError. */
function inRange<T>(type: string, text: string, temporal: string, build: () => T): T {
  try {
    return build();
  } catch (e) {
    if (e instanceof RangeError) {
      throw new DecodeError(`typedpg: the ${type} ${JSON.stringify(text)} is out of a ${temporal}'s range`);
    }
    throw e;
  }
}

/** The ISO year of PG's `year` and ` BC` (1 BC is year 0). */
function isoYear(year: string, bc: string | undefined): number {
  return bc ? 1 - Number(year) : Number(year);
}

/** A fraction of a second (`5`, `123456`) as milliseconds, microseconds and nanoseconds. */
function subseconds(fraction: string | undefined): [number, number, number] {
  const f = (fraction ?? "").padEnd(9, "0");
  return [Number(f.slice(0, 3)), Number(f.slice(3, 6)), Number(f.slice(6, 9))];
}

function parseDate(text: string): Temporal.PlainDate {
  if (text === "infinity") return new Temporal.PlainDate(275760, 9, 13);
  if (text === "-infinity") return new Temporal.PlainDate(-271821, 4, 20);
  const m = DATE_RE.exec(text);
  if (!m) throw unreadable("date", text);
  return inRange("date", text, "Temporal.PlainDate", () =>
    new Temporal.PlainDate(isoYear(m[1]!, m[4]), Number(m[2]), Number(m[3])),
  );
}

function parseTimestamp(text: string): Temporal.PlainDateTime {
  if (text === "infinity") return new Temporal.PlainDateTime(275760, 9, 13);
  if (text === "-infinity") return new Temporal.PlainDateTime(-271821, 4, 20);
  const m = TIMESTAMP_RE.exec(text);
  if (!m) throw unreadable("timestamp", text);
  return inRange("timestamp", text, "Temporal.PlainDateTime", () =>
    new Temporal.PlainDateTime(
      isoYear(m[1]!, m[8]),
      Number(m[2]),
      Number(m[3]),
      Number(m[4]),
      Number(m[5]),
      Number(m[6]),
      ...subseconds(m[7]),
    ),
  );
}

/**
 * Days from 1970-01-01 to an ISO date, in the proleptic Gregorian
 * calendar, for any year (Howard Hinnant's `days_from_civil`).
 */
function epochDays(year: number, month: number, day: number): number {
  const y = month <= 2 ? year - 1 : year;
  const era = Math.floor(y / 400);
  const yoe = y - era * 400;
  const doy = Math.floor((153 * (month + (month > 2 ? -3 : 9)) + 2) / 5) + day - 1;
  const doe = yoe * 365 + Math.floor(yoe / 4) - Math.floor(yoe / 100) + doy;
  return era * 146097 + doe - 719468;
}

function parseTimestamptz(text: string): Temporal.Instant {
  if (text === "infinity") return Temporal.Instant.fromEpochMilliseconds(8.64e15);
  if (text === "-infinity") return Temporal.Instant.fromEpochMilliseconds(-8.64e15);
  const m = TIMESTAMPTZ_RE.exec(text);
  if (!m) throw unreadable("timestamptz", text);
  // From the fields, not through a PlainDateTime: the local time of an
  // instant near the limits can be out of a PlainDateTime's range.
  const offset =
    (m[8] === "-" ? -1 : 1) * (Number(m[9]) * 3600 + Number(m[10] ?? 0) * 60 + Number(m[11] ?? 0));
  const seconds =
    epochDays(isoYear(m[1]!, m[12]), Number(m[2]), Number(m[3])) * 86400 +
    Number(m[4]) * 3600 +
    Number(m[5]) * 60 +
    Number(m[6]) -
    offset;
  const fraction = BigInt((m[7] ?? "").padEnd(9, "0"));
  return inRange("timestamptz", text, "Temporal.Instant", () =>
    new Temporal.Instant(BigInt(seconds) * 1_000_000_000n + fraction),
  );
}

function parseTime(text: string): Temporal.PlainTime {
  const m = TIME_RE.exec(text);
  if (!m) throw unreadable("time", text);
  // PG's time goes up to 24:00:00 inclusive, a PlainTime to 23:59:59.999999999.
  return inRange("time", text, "Temporal.PlainTime", () =>
    new Temporal.PlainTime(Number(m[1]), Number(m[2]), Number(m[3]), ...subseconds(m[4])),
  );
}

/** What a parameter that isn't the Temporal type it must be is, for the error. */
function describe(value: unknown): string {
  if (typeof value !== "object" || value === null) return typeof value;
  const name = (value as object).constructor?.name;
  if (!name) return "an object";
  return `${/^[AEIOU]/.test(name) ? "an" : "a"} ${name}`;
}

const pad = (n: number, width = 2) => String(n).padStart(width, "0");

/** An ISO date as PG reads it: `0044-03-15 BC` for 44 BC, `10000-01-01`. */
function formatYmd(year: number, month: number, day: number): [string, string] {
  return [`${pad(year <= 0 ? 1 - year : year, 4)}-${pad(month)}-${pad(day)}`, year <= 0 ? " BC" : ""];
}

function formatHms(t: {
  hour: number;
  minute: number;
  second: number;
  millisecond: number;
  microsecond: number;
  nanosecond: number;
}): string {
  // Every digit: PG rounds what is past its microseconds.
  return (
    `${pad(t.hour)}:${pad(t.minute)}:${pad(t.second)}.` +
    `${pad(t.millisecond, 3)}${pad(t.microsecond, 3)}${pad(t.nanosecond, 3)}`
  );
}

function formatDate(value: unknown): string {
  if (!(value instanceof Temporal.PlainDate)) {
    throw new TypeError(`typedpg: a date parameter must be a Temporal.PlainDate, not ${describe(value)}`);
  }
  // In the ISO calendar: another calendar's fields number other years.
  const d = value.withCalendar("iso8601");
  const [ymd, bc] = formatYmd(d.year, d.month, d.day);
  return ymd + bc;
}

function formatTimestamp(value: unknown): string {
  if (!(value instanceof Temporal.PlainDateTime)) {
    throw new TypeError(
      `typedpg: a timestamp parameter must be a Temporal.PlainDateTime, not ${describe(value)}`,
    );
  }
  const d = value.withCalendar("iso8601");
  const [ymd, bc] = formatYmd(d.year, d.month, d.day);
  return `${ymd} ${formatHms(d)}${bc}`;
}

function formatTimestamptz(value: unknown): string {
  let instant: Temporal.Instant;
  if (value instanceof Temporal.Instant) instant = value;
  else if (value instanceof Temporal.ZonedDateTime) instant = value.toInstant();
  else if (value instanceof Date) {
    if (Number.isNaN(value.getTime())) throw new RangeError("typedpg: an invalid Date can't be a timestamptz");
    instant = Temporal.Instant.fromEpochMilliseconds(value.getTime());
  } else {
    throw new TypeError(
      `typedpg: a timestamptz parameter must be a Temporal.Instant, a Temporal.ZonedDateTime or a Date, not ${describe(value)}`,
    );
  }
  const d = instant.toZonedDateTimeISO("UTC");
  const [ymd, bc] = formatYmd(d.year, d.month, d.day);
  return `${ymd} ${formatHms(d)}Z${bc}`;
}

function formatTime(value: unknown): string {
  if (!(value instanceof Temporal.PlainTime)) {
    throw new TypeError(`typedpg: a time parameter must be a Temporal.PlainTime, not ${describe(value)}`);
  }
  return formatHms(value);
}
