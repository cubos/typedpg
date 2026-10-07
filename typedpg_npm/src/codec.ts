// Conversion between PostgreSQL's text format and JS values. Every column is
// read as text (the adapters turn the drivers' own parsing off), so a value
// is what the generated module's type for it promises, whatever type parsers
// the application set up on its driver.

/** How a value is converted; the generator writes one per column and parameter. */
export type Codec =
  | "text"
  | "bool"
  | "number"
  | "bigint"
  | "int8number"
  | "json"
  | "timestamptz"
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

/** PostgreSQL's `interval`, field by field, as it stores it. */
export interface Interval {
  readonly months: number;
  readonly days: number;
  /** The time part; a `bigint`, as it can exceed 2^53. */
  readonly microseconds: bigint;
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
      return text;
    case "bool":
      return text === "t";
    case "number":
      // `NaN`, `Infinity` and `-Infinity` are what PG prints for those.
      return Number(text);
    case "bigint":
      return BigInt(text);
    case "int8number": {
      const n = Number(text);
      if (!Number.isSafeInteger(n)) {
        throw new DecodeError(`typedpg: int8 value ${text} is beyond what a number holds exactly; use "int8": "bigint"`);
      }
      return n;
    }
    case "json":
      return parseJson(text);
    case "timestamptz":
      return parseTimestamptz(text);
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
      return value ? "t" : "f";
    case "json":
      return JSON.stringify(value);
    case "timestamptz":
      return value instanceof Date ? formatTimestamptz(value) : String(value);
    case "bytea":
      return "\\x" + Buffer.from(value as Uint8Array).toString("hex");
    case "interval": {
      if (typeof value === "string") return value;
      const i = value as Interval;
      return `${i.months} mons ${i.days} days ${i.microseconds} microseconds`;
    }
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
  return "{" + parts.join(",") + "}";
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
 * `{a,"b c",NULL}` (with an optional `[1:3]=` bounds decoration) as an array.
 * A multidimensional array is an error, as on the Rust side: its type is the
 * one-dimensional one's.
 */
function parseArray(text: string, codec: Codec): unknown[] {
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
    const [item, quoted, next] = readItem(text, i, ",}", false);
    i = next;
    out.push(!quoted && item.toUpperCase() === "NULL" ? null : decode(codec, item));
    if (text[i] === "}") return out;
    if (text[i] !== ",") throw new DecodeError(`typedpg: malformed array: ${text}`);
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
 * PG's ISO output (`2024-01-02 03:04:05.123456-03`, the default DateStyle) as
 * a Date. Parsed by hand: the offset can carry seconds (`+00:53:28`, a
 * historical zone), which `Date.parse` rejects.
 */
function parseTimestamptz(text: string): Date {
  if (text === "infinity") return new Date(8.64e15);
  if (text === "-infinity") return new Date(-8.64e15);
  const m =
    /^(\d{4,})-(\d\d)-(\d\d)[ T](\d\d):(\d\d):(\d\d)(?:\.(\d+))?(?:([+-])(\d\d)(?::?(\d\d))?(?::?(\d\d))?)?( BC)?$/.exec(
      text,
    );
  if (!m) {
    throw new DecodeError(
      `typedpg: unreadable timestamptz ${JSON.stringify(text)}: typedpg reads PostgreSQL's ISO ` +
        `DateStyle, the default; this session uses another (SET DateStyle = 'ISO')`,
    );
  }
  let year = Number(m[1]);
  if (m[12]) year = 1 - year;
  const ms = m[7] ? Number(m[7].slice(0, 3).padEnd(3, "0")) : 0;
  const date = new Date(0);
  // `Date.UTC` would map years 0–99 to 1900–1999.
  date.setUTCFullYear(year, Number(m[2]) - 1, Number(m[3]));
  date.setUTCHours(Number(m[4]), Number(m[5]), Number(m[6]), ms);
  const offset = m[8]
    ? (m[8] === "-" ? -1 : 1) * (Number(m[9]) * 3600 + Number(m[10] ?? 0) * 60 + Number(m[11] ?? 0))
    : 0;
  return new Date(date.getTime() - offset * 1000);
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

/**
 * A Date as PG reads it: `toISOString()` writes years before 1 as
 * `-000043`, which PG rejects; it takes `0044-03-15 12:00:00Z BC`.
 */
function formatTimestamptz(d: Date): string {
  if (Number.isNaN(d.getTime())) throw new RangeError("typedpg: an invalid Date can't be a timestamptz");
  const year = d.getUTCFullYear();
  const pad = (n: number, w = 2) => String(n).padStart(w, "0");
  const y = year <= 0 ? 1 - year : year;
  return (
    `${pad(y, 4)}-${pad(d.getUTCMonth() + 1)}-${pad(d.getUTCDate())} ` +
    `${pad(d.getUTCHours())}:${pad(d.getUTCMinutes())}:${pad(d.getUTCSeconds())}.${pad(d.getUTCMilliseconds(), 3)}Z` +
    (year <= 0 ? " BC" : "")
  );
}

/** The hex format (`\x0102`, the default `bytea_output`) or the escape one. */
function parseBytea(text: string): Uint8Array {
  if (text.startsWith("\\x")) {
    return new Uint8Array(Buffer.from(text.slice(2), "hex"));
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
  return new Uint8Array(out);
}

/**
 * An interval in the `postgres` IntervalStyle (the default: `1 year 2 mons
 * -3 days 04:05:06.5`) or `iso_8601` (`P1Y2M-3DT4H5M6.5S`).
 */
function parseInterval(text: string): Interval {
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
    return { months, days, microseconds: micros };
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
  if (matched < text.length) {
    throw new DecodeError(
      `typedpg: unreadable interval ${JSON.stringify(text)}: typedpg reads the postgres and iso_8601 ` +
        `IntervalStyles (SET IntervalStyle = 'postgres')`,
    );
  }
  return { months, days, microseconds: micros };
}
