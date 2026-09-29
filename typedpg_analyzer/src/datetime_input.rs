//! Literal input validation for the datetime family — a port of PostgreSQL
//! 18's datetime input decoder (`src/backend/utils/adt/datetime.c`:
//! `ParseDateTime`, `DecodeDateTime`, `DecodeTimeOnly`, `DecodeInterval`,
//! `DecodeISO8601Interval`, …) together with the range checks of the input
//! functions built on it (`date_in`, `time_in`, `timetz_in` in date.c;
//! `timestamp_in`, `timestamptz_in`, `interval_in` in timestamp.c).
//!
//! Server settings assumed (PG's defaults, and the pg_sanity oracle's):
//! `DateStyle = ISO, MDY`, `IntervalStyle = postgres`, and the `Default`
//! `timezone_abbreviations` set. The session `TimeZone` is treated as an
//! unknown offset.
//!
//! The contract is that of [`crate::literal_input::validate`]: never reject
//! a string PG accepts, and reject with PG's verbatim message. Decoding is
//! therefore tri-state — where PG's answer depends on something we cannot
//! know at compile time, the decoder yields [`Dterr::Unsure`], which
//! accepts:
//!
//! - whether a full time zone name exists in the server's tz database
//!   (only the names present in every current tzdata install are treated
//!   as known; legacy backward-compatibility links, and unknown names under
//!   a known area such as `America/…`, are unsure);
//! - the current time of day (`'now pm'` depends on it);
//! - the session time zone / named zone offset when a timestamptz lands
//!   within a day of the timestamp range limits;
//! - hexadecimal or subnormal numbers in ISO 8601 intervals (strtod
//!   corner cases);
//! - the interval typmod, where the caller doesn't know it: PG passes an
//!   interval column's / cast's field restriction to `interval_in` (it
//!   changes how bare numbers and `mm:ss` decode) — without it an interval
//!   literal is only rejected when every field restriction rejects it the
//!   same way; a cast's known typmod goes through [`validate_interval`].

use crate::pgmsg;

/// The datetime types whose input goes through the datetime decoder.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DatetimeType {
    Date,
    Time,
    TimeTz,
    Timestamp,
    TimestampTz,
    Interval,
}

impl DatetimeType {
    /// Map a `pg_catalog` type name to its datetime input family.
    pub(crate) fn from_typname(name: &str) -> Option<Self> {
        Some(match name {
            "date" => Self::Date,
            "time" => Self::Time,
            "timetz" => Self::TimeTz,
            "timestamp" => Self::Timestamp,
            "timestamptz" => Self::TimestampTz,
            "interval" => Self::Interval,
            _ => return None,
        })
    }

    /// The type name the input function hands to `DateTimeParseError` —
    /// the SQL spelling, not `format_type`'s.
    fn msg_name(self) -> &'static str {
        match self {
            Self::Date => "date",
            Self::Time => "time",
            Self::TimeTz => "time with time zone",
            Self::Timestamp => "timestamp",
            Self::TimestampTz => "timestamp with time zone",
            Self::Interval => "interval",
        }
    }
}

/// PG's DTERR_* codes, plus the input functions' own range errors and the
/// "can't know" outcome.
#[derive(Debug, Clone, PartialEq)]
enum Dterr {
    BadFormat,
    FieldOverflow,
    MdFieldOverflow,
    IntervalOverflow,
    TzdispOverflow,
    /// DTERR_BAD_TIMEZONE, carrying the (lowercased) zone field.
    BadTimezone(String),
    DateOutOfRange,
    TimestampOutOfRange,
    IntervalOutOfRange,
    /// The outcome depends on server state we don't model — accept.
    Unsure,
}

type DtResult<T> = Result<T, Dterr>;

/// Validate `content` as input to the datetime type `ty`: `Ok(())` when PG
/// accepts it (or we can't tell), `Err(message)` with PG's verbatim error.
pub(crate) fn validate(content: &str, ty: DatetimeType) -> Result<(), String> {
    let outcome = match ty {
        DatetimeType::Interval => interval_in(content),
        DatetimeType::Date => date_in(content),
        DatetimeType::Time | DatetimeType::TimeTz => time_in(content),
        DatetimeType::Timestamp => timestamp_in(content, false),
        DatetimeType::TimestampTz => timestamp_in(content, true),
    };
    match outcome {
        Ok(()) | Err(Dterr::Unsure) => Ok(()),
        Err(e) => Err(message(&e, content, ty)),
    }
}

/// `interval_in` with a known typmod (`-1` for none): the field restriction
/// the typmod carries (`INTERVAL_RANGE`) decides how bare numbers and
/// `mm:ss` fields decode, so with it known the literal is decided exactly
/// instead of only when every restriction agrees ([`validate`]).
pub(crate) fn validate_interval(content: &str, typmod: i32) -> Result<(), String> {
    let range = if typmod < 0 {
        INTERVAL_FULL_RANGE
    } else {
        (typmod >> 16) & INTERVAL_FULL_RANGE
    };
    match interval_in_range(content, range) {
        Ok(()) | Err(Dterr::Unsure) => Ok(()),
        Err(e) => Err(message(&e, content, DatetimeType::Interval)),
    }
}

/// `DateTimeParseError` (datetime.c) plus the input functions' range
/// messages.
fn message(e: &Dterr, content: &str, ty: DatetimeType) -> String {
    match e {
        Dterr::FieldOverflow | Dterr::MdFieldOverflow => {
            format!("date/time field value out of range: \"{content}\"")
        }
        Dterr::IntervalOverflow => format!("interval field value out of range: \"{content}\""),
        Dterr::TzdispOverflow => {
            format!("time zone displacement out of range: \"{content}\"")
        }
        Dterr::BadTimezone(tz) => format!("time zone \"{tz}\" not recognized"),
        Dterr::DateOutOfRange => format!("date out of range: \"{content}\""),
        Dterr::TimestampOutOfRange => format!("timestamp out of range: \"{content}\""),
        Dterr::IntervalOutOfRange => "interval out of range".to_string(),
        Dterr::BadFormat | Dterr::Unsure => {
            pgmsg::invalid_input_syntax_for_type(ty.msg_name(), content)
        }
    }
}

// ─── constants (datetime.h / timestamp.h) ───────────────────────────────────

// Field classes assigned by ParseDateTime.
const DTK_NUMBER: u8 = 0;
const DTK_STRING: u8 = 1;
const DTK_DATE: u8 = 2;
const DTK_TIME: u8 = 3;
const DTK_TZ: u8 = 4;
const DTK_SPECIAL: u8 = 6;

// Token types (datetkn.type) — also the bit positions of the field masks.
const RESERV: i32 = 0;
const MONTH: i32 = 1;
const YEAR: i32 = 2;
const DAY: i32 = 3;
const TZ: i32 = 5;
const DTZ: i32 = 6;
const DYNTZ: i32 = 7;
const IGNORE_DTF: i32 = 8;
const AMPM: i32 = 9;
const HOUR: i32 = 10;
const MINUTE: i32 = 11;
const SECOND: i32 = 12;
const MILLISECOND: i32 = 13;
const MICROSECOND: i32 = 14;
const DOY: i32 = 15;
const DOW: i32 = 16;
const UNITS: i32 = 17;
const ADBC: i32 = 18;
const AGO: i32 = 19;
const ISOTIME: i32 = 23;
const WEEK: i32 = 24;
const DECADE: i32 = 25;
const CENTURY: i32 = 26;
const MILLENNIUM: i32 = 27;
const DTZMOD: i32 = 28;
const UNKNOWN_FIELD: i32 = 31;

// Token values (datetkn.value) / dtype codes.
const DTK_DATE_V: i32 = 2;
const DTK_TIME_V: i32 = 3;
const DTK_TZ_V: i32 = 4;
const DTK_EARLY: i32 = 9;
const DTK_LATE: i32 = 10;
const DTK_EPOCH: i32 = 11;
const DTK_NOW: i32 = 12;
const DTK_YESTERDAY: i32 = 13;
const DTK_TODAY: i32 = 14;
const DTK_TOMORROW: i32 = 15;
const DTK_ZULU: i32 = 16;
const DTK_DELTA: i32 = 17;
const DTK_SECOND: i32 = 18;
const DTK_MINUTE: i32 = 19;
const DTK_HOUR: i32 = 20;
const DTK_DAY: i32 = 21;
const DTK_WEEK: i32 = 22;
const DTK_MONTH: i32 = 23;
const DTK_QUARTER: i32 = 24;
const DTK_YEAR: i32 = 25;
const DTK_DECADE: i32 = 26;
const DTK_CENTURY: i32 = 27;
const DTK_MILLENNIUM: i32 = 28;
const DTK_MILLISEC: i32 = 29;
const DTK_MICROSEC: i32 = 30;
const DTK_JULIAN: i32 = 31;
const DTK_DOW: i32 = 32;
const DTK_DOY: i32 = 33;
const DTK_TZ_HOUR: i32 = 34;
const DTK_TZ_MINUTE: i32 = 35;
const DTK_ISOYEAR: i32 = 36;
const DTK_ISODOW: i32 = 37;

const AM: i32 = 0;
const PM: i32 = 1;
const HR24: i32 = 2;
const AD: i32 = 0;
const BC: i32 = 1;

const fn dtk_m(t: i32) -> i32 {
    1 << t
}
const DTK_ALL_SECS_M: i32 = dtk_m(SECOND) | dtk_m(MILLISECOND) | dtk_m(MICROSECOND);
const DTK_DATE_M: i32 = dtk_m(YEAR) | dtk_m(MONTH) | dtk_m(DAY);
const DTK_TIME_M: i32 = dtk_m(HOUR) | dtk_m(MINUTE) | DTK_ALL_SECS_M;

const MAXDATELEN: usize = 128;
const MAXDATEFIELDS: usize = 25;
const TOKMAXLEN: usize = 10;
const MAX_TZDISP_HOUR: i32 = 15;

const USECS_PER_DAY: i64 = 86_400_000_000;
const USECS_PER_HOUR: i64 = 3_600_000_000;
const USECS_PER_MINUTE: i64 = 60_000_000;
const USECS_PER_SEC: i64 = 1_000_000;
const MONTHS_PER_YEAR: i32 = 12;
const DAYS_PER_MONTH: i32 = 30;

const POSTGRES_EPOCH_JDATE: i64 = 2_451_545;
const DATETIME_MIN_JULIAN: i64 = 0;
const DATE_END_JULIAN: i64 = 2_147_483_494;
const MIN_TIMESTAMP: i64 = -211_813_488_000_000_000;
const END_TIMESTAMP: i64 = 9_223_371_331_200_000_000;

// Interval typmod field masks (INTERVAL_MASK), for the `range` argument.
const fn interval_mask(b: i32) -> i32 {
    1 << b
}
const INTERVAL_FULL_RANGE: i32 = 0x7FFF;

// ─── C library helpers ──────────────────────────────────────────────────────

/// C `isspace` in the server's (ASCII-only for our purposes) ctype.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r')
}

/// Byte at `i`, or NUL past the end — mirrors walking a C string.
fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

/// C `strtol(s, &end, 10)` on a 64-bit `long`: `(value, end, erange)`. With
/// no digits the end is 0 (no conversion) and the value 0.
fn strtol(s: &[u8]) -> (i64, usize, bool) {
    let mut i = 0;
    while i < s.len() && is_space(s[i]) {
        i += 1;
    }
    let neg = match at(s, i) {
        b'-' => {
            i += 1;
            true
        }
        b'+' => {
            i += 1;
            false
        }
        _ => false,
    };
    let start = i;
    let mut acc: i128 = 0;
    while i < s.len() && s[i].is_ascii_digit() {
        acc = (acc * 10 + i128::from(s[i] - b'0')).min(i128::from(u64::MAX) + 1);
        i += 1;
    }
    if i == start {
        return (0, 0, false);
    }
    let v = if neg { -acc } else { acc };
    if v > i128::from(i64::MAX) {
        (i64::MAX, i, true)
    } else if v < i128::from(i64::MIN) {
        (i64::MIN, i, true)
    } else {
        (v as i64, i, false)
    }
}

/// PG's `strtoint` (string.c): `strtol` plus an `int` range check.
fn strtoint(s: &[u8]) -> (i32, usize, bool) {
    let (v, end, erange) = strtol(s);
    (v as i32, end, erange || v != i64::from(v as i32))
}

/// glibc `atoi`: `(int) strtol(s, NULL, 10)`.
fn atoi(s: &[u8]) -> i32 {
    strtol(s).0 as i32
}

/// `date2j` (datetime.c), with C's wrapping `int` arithmetic.
fn date2j(year: i32, month: i32, day: i32) -> i32 {
    let (mut year, mut month) = (year, month);
    if month > 2 {
        month = month.wrapping_add(1);
        year = year.wrapping_add(4800);
    } else {
        month = month.wrapping_add(13);
        year = year.wrapping_add(4799);
    }
    let century = year / 100;
    let mut julian = year.wrapping_mul(365).wrapping_sub(32167);
    julian = julian.wrapping_add(year / 4 - century + century / 4);
    julian
        .wrapping_add(7834i32.wrapping_mul(month) / 256)
        .wrapping_add(day)
}

/// `j2date` (datetime.c), with its unsigned arithmetic.
fn j2date(jd: i32) -> (i32, i32, i32) {
    let mut julian = jd as u32;
    julian = julian.wrapping_add(32044);
    let mut quad = julian / 146_097;
    let extra = julian
        .wrapping_sub(quad.wrapping_mul(146_097))
        .wrapping_mul(4)
        .wrapping_add(3);
    julian = julian
        .wrapping_add(60)
        .wrapping_add(quad.wrapping_mul(3))
        .wrapping_add(extra / 146_097);
    quad = julian / 1461;
    julian = julian.wrapping_sub(quad.wrapping_mul(1461));
    let mut y = (julian.wrapping_mul(4) / 1461) as i32;
    julian = if y != 0 {
        julian.wrapping_add(305) % 365
    } else {
        julian.wrapping_add(306) % 366
    }
    .wrapping_add(123);
    y = (y as u32).wrapping_add(quad.wrapping_mul(4)) as i32;
    let year = y.wrapping_sub(4800);
    quad = julian.wrapping_mul(2141) / 65536;
    let day = julian.wrapping_sub(7834u32.wrapping_mul(quad) / 256) as i32;
    let month = (quad.wrapping_add(10) % 12 + 1) as i32;
    (year, month, day)
}

fn isleap(y: i32) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

const DAY_TAB: [[i32; 13]; 2] = [
    [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31, 0],
    [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31, 0],
];

/// `IS_VALID_JULIAN` (timestamp.h).
fn is_valid_julian(y: i32, m: i32) -> bool {
    (y > -4713 || (y == -4713 && m >= 11)) && (y < 5_874_898 || (y == 5_874_898 && m < 6))
}

/// Keyword-table match with `datebsearch`'s `strncmp(key, token,
/// TOKMAXLEN)` semantics: a key longer than `TOKMAXLEN` matches a token
/// truncated to that length.
fn tok_eq(key: &[u8], token: &str) -> bool {
    let k = &key[..key.len().min(TOKMAXLEN)];
    let t = token.as_bytes();
    &t[..t.len().min(TOKMAXLEN)] == k
}

fn tbl_lookup(key: &[u8], tbl: &[(&str, i32, i32)]) -> Option<(i32, i32)> {
    tbl.iter()
        .find(|(tok, _, _)| tok_eq(key, tok))
        .map(|&(_, ty, val)| (ty, val))
}

/// `DecodeSpecial`: the `datetktbl` keywords.
fn decode_special(key: &[u8]) -> (i32, i32) {
    tbl_lookup(key, DATETKTBL).unwrap_or((UNKNOWN_FIELD, 0))
}

/// `DecodeUnits`: the `deltatktbl` interval units.
fn decode_units(key: &[u8]) -> (i32, i32) {
    tbl_lookup(key, DELTATKTBL).unwrap_or((UNKNOWN_FIELD, 0))
}

/// A zone abbreviation of the `Default` timezone_abbreviations set.
#[derive(Clone, Copy)]
enum Abbrev {
    /// Fixed offset, standard time (seconds east of UTC).
    Tz(i32),
    /// Fixed offset, daylight time.
    Dtz(i32),
    /// Offset depends on the date (resolved through a tz database zone).
    Dyn,
}

/// `DecodeTimezoneAbbrev`: `(type, value)` for a known abbreviation. The
/// session time zone's own abbreviations are consulted first in PG 18;
/// under the assumed `Etc/UTC` session zone that is just `UTC`, which the
/// `Default` set already carries with the same meaning.
fn decode_timezone_abbrev(key: &[u8]) -> Option<(i32, Option<i32>)> {
    ZONE_ABBREVS
        .iter()
        .find(|(tok, _)| tok_eq(key, tok))
        .map(|&(_, a)| match a {
            Abbrev::Tz(off) => (TZ, Some(off)),
            Abbrev::Dtz(off) => (DTZ, Some(off)),
            Abbrev::Dyn => (DYNTZ, None),
        })
}

// ─── time zone names (pg_tzset, pgtz.c) ─────────────────────────────────────

/// What `pg_tzset` would make of a zone name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ZoneLookup {
    /// A zone that loads; `fixed` is `pg_get_timezone_offset`'s answer.
    Known { fixed: bool },
    /// Not a zone on any server.
    NotFound,
    /// Depends on the server's tz database.
    Unsure,
}

/// Top-level directories of the tz database, plus the `posix/` and
/// `right/` trees some distributions ship.
const TZ_AREAS: &[&str] = &[
    "africa",
    "america",
    "antarctica",
    "arctic",
    "asia",
    "atlantic",
    "australia",
    "brazil",
    "canada",
    "chile",
    "etc",
    "europe",
    "indian",
    "mexico",
    "pacific",
    "posix",
    "right",
    "us",
];

/// `pg_tzset`: `GMT`, then a tz database file (case-insensitive), then a
/// POSIX-style zone spec (`tzparse`, localtime.c). `name` is lowercase.
fn pg_tzset(name: &[u8]) -> ZoneLookup {
    if name.len() > 255 {
        return ZoneLookup::NotFound;
    }
    let lower = String::from_utf8_lossy(name);
    if lower == "gmt" {
        return ZoneLookup::Known { fixed: true };
    }
    if KNOWN_ZONES.binary_search(&lower.as_ref()).is_ok() {
        // Verified against PG 18 (`'12:00 <zone>'::timetz` for every
        // zone): exactly the `Etc/` zones and the UTC/Factory aliases have
        // a single UTC offset.
        let fixed = lower.starts_with("etc/") || matches!(lower.as_ref(), "utc" | "factory");
        return ZoneLookup::Known { fixed };
    }
    let upper = name.to_ascii_uppercase();
    if let Some(fixed) = tzparse(&upper) {
        return ZoneLookup::Known { fixed };
    }
    // Legacy backward-compatibility links (`Japan`, `US/Eastern`, …) are
    // only installed by some distributions; `localtime` / `posixrules`
    // are host-dependent; a new zone under a known area may postdate our
    // list.
    let area_known = lower
        .split_once('/')
        .is_some_and(|(area, _)| TZ_AREAS.contains(&area));
    if LEGACY_ZONES.binary_search(&lower.as_ref()).is_ok()
        || matches!(lower.as_ref(), "localtime" | "posixrules")
        || area_known
    {
        return ZoneLookup::Unsure;
    }
    ZoneLookup::NotFound
}

/// `tzparse(name, sp, false)` (localtime.c) on an uppercased POSIX zone
/// spec: `Some(fixed)` when it parses (fixed = no DST part).
fn tzparse(name: &[u8]) -> Option<bool> {
    fn getzname(s: &[u8], mut i: usize) -> usize {
        while i < s.len() && !s[i].is_ascii_digit() && !matches!(s[i], b',' | b'-' | b'+') {
            i += 1;
        }
        i
    }
    fn getqzname(s: &[u8], mut i: usize) -> usize {
        while i < s.len() && s[i] != b'>' {
            i += 1;
        }
        i
    }
    fn getnum(s: &[u8], mut i: usize, min: i32, max: i32) -> Option<usize> {
        if !at(s, i).is_ascii_digit() {
            return None;
        }
        let mut num = 0i32;
        while at(s, i).is_ascii_digit() {
            num = num * 10 + i32::from(s[i] - b'0');
            if num > max {
                return None;
            }
            i += 1;
        }
        (num >= min).then_some(i)
    }
    fn getoffset(s: &[u8], mut i: usize) -> Option<usize> {
        if matches!(at(s, i), b'-' | b'+') {
            i += 1;
        }
        // getsecs: hours up to HOURSPERDAY * DAYSPERWEEK - 1.
        i = getnum(s, i, 0, 24 * 7 - 1)?;
        if at(s, i) == b':' {
            i = getnum(s, i + 1, 0, 59)?;
            if at(s, i) == b':' {
                i = getnum(s, i + 1, 0, 60)?;
            }
        }
        Some(i)
    }
    let mut i = if at(name, 0) == b'<' {
        let j = getqzname(name, 1);
        if at(name, j) != b'>' {
            return None;
        }
        j + 1
    } else {
        getzname(name, 0)
    };
    if i >= name.len() {
        return None;
    }
    i = getoffset(name, i)?;
    if i >= name.len() {
        return Some(true);
    }
    let dst_start = i;
    if at(name, i) == b'<' {
        let j = getqzname(name, i + 1);
        if at(name, j) != b'>' || j == i + 1 {
            return None;
        }
        i = j + 1;
    } else {
        i = getzname(name, i);
        if i == dst_start {
            return None;
        }
    }
    if i < name.len() && !matches!(name[i], b',' | b';') {
        i = getoffset(name, i)?;
    }
    if i >= name.len() {
        // Default DST rules (TZDEFRULESTRING).
        return Some(false);
    }
    // Explicit transition rules: `,`/`;` never survive ParseDateTime's
    // tokenizer, so this is unreachable from literal input.
    matches!(name[i], b',' | b';').then_some(false)
}

// ─── tokenizer (ParseDateTime) ──────────────────────────────────────────────

struct Field {
    text: Vec<u8>,
    ftype: u8,
}

/// `ParseDateTime`: split the input into typed fields, lowercasing text.
/// `buflen` is the caller's work buffer size — PG rejects input whose
/// fields (plus a NUL each) don't fit.
fn parse_date_time(s: &[u8], buflen: usize) -> DtResult<Vec<Field>> {
    let mut fields: Vec<Field> = Vec::new();
    let mut bufp = 0usize;
    let mut cp = 0usize;
    // APPEND_CHAR: fails when the buffer can't take one more character.
    macro_rules! append {
        ($buf:ident, $c:expr) => {{
            if bufp + 1 >= buflen {
                return Err(Dterr::BadFormat);
            }
            bufp += 1;
            $buf.push($c);
        }};
    }
    while cp < s.len() {
        let c = s[cp];
        if is_space(c) {
            cp += 1;
            continue;
        }
        if fields.len() >= MAXDATEFIELDS {
            return Err(Dterr::BadFormat);
        }
        let mut buf: Vec<u8> = Vec::new();
        let ftype;
        if c.is_ascii_digit() {
            append!(buf, c);
            cp += 1;
            while at(s, cp).is_ascii_digit() {
                append!(buf, s[cp]);
                cp += 1;
            }
            let c2 = at(s, cp);
            if c2 == b':' {
                ftype = DTK_TIME;
                append!(buf, c2);
                cp += 1;
                while matches!(at(s, cp), b'0'..=b'9' | b':' | b'.') {
                    append!(buf, s[cp]);
                    cp += 1;
                }
            } else if matches!(c2, b'-' | b'/' | b'.') {
                let delim = c2;
                append!(buf, c2);
                cp += 1;
                if at(s, cp).is_ascii_digit() {
                    let mut ft = if delim == b'.' { DTK_NUMBER } else { DTK_DATE };
                    while at(s, cp).is_ascii_digit() {
                        append!(buf, s[cp]);
                        cp += 1;
                    }
                    // Insist that the delimiters match for a three-field date.
                    if at(s, cp) == delim {
                        ft = DTK_DATE;
                        append!(buf, delim);
                        cp += 1;
                        while at(s, cp).is_ascii_digit() || at(s, cp) == delim {
                            append!(buf, s[cp]);
                            cp += 1;
                        }
                    }
                    ftype = ft;
                } else {
                    ftype = DTK_DATE;
                    while at(s, cp).is_ascii_alphanumeric() || at(s, cp) == delim {
                        append!(buf, s[cp].to_ascii_lowercase());
                        cp += 1;
                    }
                }
            } else {
                ftype = DTK_NUMBER;
            }
        } else if c == b'.' {
            append!(buf, c);
            cp += 1;
            while at(s, cp).is_ascii_digit() {
                append!(buf, s[cp]);
                cp += 1;
            }
            ftype = DTK_NUMBER;
        } else if c.is_ascii_alphabetic() {
            let mut ft = DTK_STRING;
            append!(buf, c.to_ascii_lowercase());
            cp += 1;
            while at(s, cp).is_ascii_alphabetic() {
                append!(buf, s[cp].to_ascii_lowercase());
                cp += 1;
            }
            // Dates can embed `-`/`/`/`.`; a zone name can also embed `+`,
            // `_`, `:` — unless what we have so far is a known keyword.
            let next = at(s, cp);
            let is_date = if matches!(next, b'-' | b'/' | b'.') {
                true
            } else if next == b'+' || next.is_ascii_digit() {
                tbl_lookup(&buf, DATETKTBL).is_none()
            } else {
                false
            };
            if is_date {
                ft = DTK_DATE;
                loop {
                    append!(buf, s[cp].to_ascii_lowercase());
                    cp += 1;
                    let n = at(s, cp);
                    if !(matches!(n, b'+' | b'-' | b'/' | b'_' | b'.' | b':')
                        || n.is_ascii_alphanumeric())
                    {
                        break;
                    }
                }
            }
            ftype = ft;
        } else if c == b'+' || c == b'-' {
            append!(buf, c);
            cp += 1;
            while cp < s.len() && is_space(s[cp]) {
                cp += 1;
            }
            let n = at(s, cp);
            if n.is_ascii_digit() {
                ftype = DTK_TZ;
                append!(buf, n);
                cp += 1;
                while matches!(at(s, cp), b'0'..=b'9' | b':' | b'.' | b'-') {
                    append!(buf, s[cp]);
                    cp += 1;
                }
            } else if n.is_ascii_alphabetic() {
                ftype = DTK_SPECIAL;
                append!(buf, n.to_ascii_lowercase());
                cp += 1;
                while at(s, cp).is_ascii_alphabetic() {
                    append!(buf, s[cp].to_ascii_lowercase());
                    cp += 1;
                }
            } else {
                return Err(Dterr::BadFormat);
            }
        } else if c.is_ascii_punctuation() {
            // Other punctuation is just a delimiter.
            cp += 1;
            continue;
        } else {
            return Err(Dterr::BadFormat);
        }
        // The field's terminating NUL (not bounds-checked in PG either).
        bufp += 1;
        fields.push(Field { text: buf, ftype });
    }
    Ok(fields)
}

/// `ParseFraction`: `s` starts at the decimal point.
fn parse_fraction(s: &[u8]) -> DtResult<f64> {
    if s.len() == 1 {
        return Ok(0.0);
    }
    if !s[1..].iter().all(u8::is_ascii_digit) {
        return Err(Dterr::BadFormat);
    }
    let text = format!("0{}", String::from_utf8_lossy(s));
    text.parse::<f64>().map_err(|_| Dterr::BadFormat)
}

/// `ParseFractionalSecond`: the fraction as rounded microseconds.
fn parse_fractional_second(s: &[u8]) -> DtResult<i32> {
    let frac = parse_fraction(s)?;
    Ok((frac * 1_000_000.0).round_ties_even() as i32)
}
// ─── field decoders ─────────────────────────────────────────────────────────

/// The subset of `struct pg_tm` the decoder fills.
#[derive(Default, Clone, Copy)]
struct Tm {
    year: i32,
    mon: i32,
    mday: i32,
    hour: i32,
    min: i32,
    sec: i32,
    yday: i32,
}

/// `DecodeNumberField`: a concatenated date (`yymmdd`, `yyyymmdd`, …) or
/// time (`hhmm`, `hhmmss[.frac]`). Returns the DTK code it decoded as.
fn decode_number_field(
    s: &[u8],
    fmask: i32,
    tmask: &mut i32,
    tm: &mut Tm,
    fsec: &mut i32,
    is2digits: &mut bool,
) -> DtResult<i32> {
    if !s.iter().all(|c| c.is_ascii_digit() || *c == b'.') {
        return Err(Dterr::BadFormat);
    }
    let mut s = s;
    if let Some(p) = s.iter().position(|&c| c == b'.') {
        *fsec = parse_fractional_second(&s[p..])?;
        s = &s[..p];
    } else if (fmask & DTK_DATE_M) != DTK_DATE_M && s.len() >= 6 {
        let len = s.len();
        *tmask = DTK_DATE_M;
        tm.mday = atoi(&s[len - 2..]);
        tm.mon = atoi(&s[len - 4..len - 2]);
        tm.year = atoi(&s[..len - 4]);
        if len - 4 == 2 {
            *is2digits = true;
        }
        return Ok(DTK_DATE_V);
    }
    if (fmask & DTK_TIME_M) != DTK_TIME_M {
        if s.len() == 6 {
            *tmask = DTK_TIME_M;
            tm.sec = atoi(&s[4..6]);
            tm.min = atoi(&s[2..4]);
            tm.hour = atoi(&s[..2]);
            return Ok(DTK_TIME_V);
        } else if s.len() == 4 {
            *tmask = DTK_TIME_M;
            tm.sec = 0;
            tm.min = atoi(&s[2..4]);
            tm.hour = atoi(&s[..2]);
            return Ok(DTK_TIME_V);
        }
    }
    Err(Dterr::BadFormat)
}

/// `DecodeNumber`: a plain numeric field interpreted in context
/// (DateOrder = MDY).
fn decode_number(
    s: &[u8],
    have_text_month: bool,
    fmask: i32,
    tmask: &mut i32,
    tm: &mut Tm,
    fsec: &mut i32,
    is2digits: &mut bool,
) -> DtResult<()> {
    let flen = s.len();
    *tmask = 0;
    let (val, end, erange) = strtoint(s);
    if erange {
        return Err(Dterr::FieldOverflow);
    }
    if end == 0 {
        return Err(Dterr::BadFormat);
    }
    if at(s, end) == b'.' {
        // More than two digits before the point: a date or run-together
        // time (2001.360, 20011225, 040506.789).
        if end > 2 {
            decode_number_field(s, fmask | DTK_DATE_M, tmask, tm, fsec, is2digits)?;
            return Ok(());
        }
        *fsec = parse_fractional_second(&s[end..])?;
    } else if end != s.len() {
        return Err(Dterr::BadFormat);
    }

    // Special case for day of year.
    if flen == 3 && (fmask & DTK_DATE_M) == dtk_m(YEAR) && (1..=366).contains(&val) {
        *tmask = dtk_m(DOY) | dtk_m(MONTH) | dtk_m(DAY);
        tm.yday = val;
        return Ok(());
    }

    match fmask & DTK_DATE_M {
        0 => {
            if flen >= 3 {
                *tmask = dtk_m(YEAR);
                tm.year = val;
            } else {
                *tmask = dtk_m(MONTH);
                tm.mon = val;
            }
        }
        m if m == dtk_m(YEAR) => {
            *tmask = dtk_m(MONTH);
            tm.mon = val;
        }
        m if m == dtk_m(MONTH) => {
            if have_text_month && flen >= 3 {
                *tmask = dtk_m(YEAR);
                tm.year = val;
            } else {
                *tmask = dtk_m(DAY);
                tm.mday = val;
            }
        }
        m if m == dtk_m(YEAR) | dtk_m(MONTH) => {
            if have_text_month && flen >= 3 && *is2digits {
                *tmask = dtk_m(DAY);
                tm.mday = tm.year;
                tm.year = val;
                *is2digits = false;
            } else {
                *tmask = dtk_m(DAY);
                tm.mday = val;
            }
        }
        m if m == dtk_m(DAY) => {
            *tmask = dtk_m(MONTH);
            tm.mon = val;
        }
        m if m == dtk_m(MONTH) | dtk_m(DAY) => {
            *tmask = dtk_m(YEAR);
            tm.year = val;
        }
        DTK_DATE_M => {
            decode_number_field(s, fmask, tmask, tm, fsec, is2digits)?;
            return Ok(());
        }
        _ => return Err(Dterr::BadFormat),
    }
    if *tmask == dtk_m(YEAR) {
        *is2digits = flen <= 2;
    }
    Ok(())
}

/// `DecodeDate`: a date with delimiters (`2024-01-15`, `jan-15-2024`, …).
fn decode_date(
    s: &[u8],
    fmask: i32,
    tmask: &mut i32,
    is2digits: &mut bool,
    tm: &mut Tm,
) -> DtResult<()> {
    let mut fmask = fmask;
    *tmask = 0;
    let mut fields: Vec<&[u8]> = Vec::new();
    let mut i = 0;
    while i < s.len() && fields.len() < MAXDATEFIELDS {
        while i < s.len() && !s[i].is_ascii_alphanumeric() {
            i += 1;
        }
        if i >= s.len() {
            return Err(Dterr::BadFormat);
        }
        let start = i;
        if s[i].is_ascii_digit() {
            while i < s.len() && s[i].is_ascii_digit() {
                i += 1;
            }
        } else {
            while i < s.len() && s[i].is_ascii_alphabetic() {
                i += 1;
            }
        }
        fields.push(&s[start..i]);
        // The character after the run is overwritten with the NUL.
        if i < s.len() {
            i += 1;
        }
    }

    // Text fields first: those are unambiguous months.
    let mut have_text_month = false;
    let mut done = vec![false; fields.len()];
    for (k, f) in fields.iter().enumerate() {
        if f[0].is_ascii_alphabetic() {
            let (ty, val) = decode_special(f);
            if ty == IGNORE_DTF {
                continue;
            }
            let dmask = dtk_m(ty);
            if ty != MONTH {
                return Err(Dterr::BadFormat);
            }
            tm.mon = val;
            have_text_month = true;
            if fmask & dmask != 0 {
                return Err(Dterr::BadFormat);
            }
            fmask |= dmask;
            *tmask |= dmask;
            done[k] = true;
        }
    }
    for (k, f) in fields.iter().enumerate() {
        if done[k] {
            continue;
        }
        let mut dmask = 0;
        let mut fsec = 0;
        decode_number(
            f,
            have_text_month,
            fmask,
            &mut dmask,
            tm,
            &mut fsec,
            is2digits,
        )?;
        if fmask & dmask != 0 {
            return Err(Dterr::BadFormat);
        }
        fmask |= dmask;
        *tmask |= dmask;
    }
    if (fmask & !(dtk_m(DOY) | dtk_m(TZ))) != DTK_DATE_M {
        return Err(Dterr::BadFormat);
    }
    Ok(())
}

/// `ValidateDate`: year/BC/2-digit adjustments, day-of-year, and month /
/// day range checks.
fn validate_date(
    fmask: i32,
    isjulian: bool,
    is2digits: bool,
    bc: bool,
    tm: &mut Tm,
) -> DtResult<()> {
    if fmask & dtk_m(YEAR) != 0 {
        if isjulian {
            // tm.year is already correct.
        } else if bc {
            if tm.year <= 0 {
                return Err(Dterr::FieldOverflow);
            }
            tm.year = -(tm.year - 1);
        } else if is2digits {
            if tm.year < 0 {
                return Err(Dterr::FieldOverflow);
            }
            if tm.year < 70 {
                tm.year += 2000;
            } else if tm.year < 100 {
                tm.year += 1900;
            }
        } else if tm.year <= 0 {
            return Err(Dterr::FieldOverflow);
        }
    }
    if fmask & dtk_m(DOY) != 0 {
        let (y, m, d) = j2date(date2j(tm.year, 1, 1).wrapping_add(tm.yday).wrapping_sub(1));
        tm.year = y;
        tm.mon = m;
        tm.mday = d;
    }
    if fmask & dtk_m(MONTH) != 0 && !(1..=MONTHS_PER_YEAR).contains(&tm.mon) {
        return Err(Dterr::MdFieldOverflow);
    }
    if fmask & dtk_m(DAY) != 0 && !(1..=31).contains(&tm.mday) {
        return Err(Dterr::MdFieldOverflow);
    }
    if (fmask & DTK_DATE_M) == DTK_DATE_M
        && tm.mday > DAY_TAB[usize::from(isleap(tm.year))][(tm.mon - 1) as usize]
    {
        return Err(Dterr::FieldOverflow);
    }
    Ok(())
}

/// Hour / minute / second / microsecond as decoded by `DecodeTimeCommon`.
struct Itm {
    hour: i64,
    min: i32,
    sec: i32,
    usec: i32,
}

const MINUTE_TO_SECOND: i32 = interval_mask(MINUTE) | interval_mask(SECOND);

/// `DecodeTimeCommon`: `hh:mm[:ss[.frac]]` (or `mm:ss.frac`).
fn decode_time_common(s: &[u8], range: i32) -> DtResult<Itm> {
    let (hour, end, erange) = strtol(s);
    if erange {
        return Err(Dterr::FieldOverflow);
    }
    if at(s, end) != b':' {
        return Err(Dterr::BadFormat);
    }
    let mut itm = Itm {
        hour,
        min: 0,
        sec: 0,
        usec: 0,
    };
    let rest = &s[end + 1..];
    let (min, e, erange) = strtoint(rest);
    if erange {
        return Err(Dterr::FieldOverflow);
    }
    itm.min = min;
    let cp = end + 1 + e;
    let mut fsec = 0i32;
    match at(s, cp) {
        0 => {
            itm.sec = 0;
            if range == MINUTE_TO_SECOND {
                if itm.hour > i64::from(i32::MAX) || itm.hour < i64::from(i32::MIN) {
                    return Err(Dterr::FieldOverflow);
                }
                itm.sec = itm.min;
                itm.min = itm.hour as i32;
                itm.hour = 0;
            }
        }
        b'.' => {
            fsec = parse_fractional_second(&s[cp..])?;
            if itm.hour > i64::from(i32::MAX) || itm.hour < i64::from(i32::MIN) {
                return Err(Dterr::FieldOverflow);
            }
            itm.sec = itm.min;
            itm.min = itm.hour as i32;
            itm.hour = 0;
        }
        b':' => {
            let (sec, e2, erange) = strtoint(&s[cp + 1..]);
            if erange {
                return Err(Dterr::FieldOverflow);
            }
            itm.sec = sec;
            let cp2 = cp + 1 + e2;
            if at(s, cp2) == b'.' {
                fsec = parse_fractional_second(&s[cp2..])?;
            } else if cp2 != s.len() {
                return Err(Dterr::BadFormat);
            }
        }
        _ => return Err(Dterr::BadFormat),
    }
    if itm.hour < 0
        || itm.min < 0
        || itm.min > 59
        || itm.sec < 0
        || itm.sec > 60
        || fsec < 0
        || i64::from(fsec) > USECS_PER_SEC
    {
        return Err(Dterr::FieldOverflow);
    }
    itm.usec = fsec;
    Ok(itm)
}

/// `DecodeTime`: the timestamp flavour of `DecodeTimeCommon`.
fn decode_time(s: &[u8], tm: &mut Tm, fsec: &mut i32) -> DtResult<()> {
    let itm = decode_time_common(s, INTERVAL_FULL_RANGE)?;
    if itm.hour > i64::from(i32::MAX) {
        return Err(Dterr::FieldOverflow);
    }
    tm.hour = itm.hour as i32;
    tm.min = itm.min;
    tm.sec = itm.sec;
    *fsec = itm.usec;
    Ok(())
}

/// `time_overflows` (date.c).
fn time_overflows(hour: i32, min: i32, sec: i32, fsec: i32) -> bool {
    if !(0..=24).contains(&hour)
        || !(0..60).contains(&min)
        || !(0..=60).contains(&sec)
        || fsec < 0
        || i64::from(fsec) > USECS_PER_SEC
    {
        return true;
    }
    ((i64::from(hour) * 60 + i64::from(min)) * 60 + i64::from(sec)) * USECS_PER_SEC
        + i64::from(fsec)
        > USECS_PER_DAY
}

/// `DecodeTimezone`: a numeric `±hh[:mm[:ss]]` / `±hhmm` zone. Returns
/// PG's `*tzp` (seconds *west* of UTC).
fn decode_timezone(s: &[u8]) -> DtResult<i32> {
    let sign = at(s, 0);
    if sign != b'+' && sign != b'-' {
        return Err(Dterr::BadFormat);
    }
    let (mut hr, e, erange) = strtoint(&s[1..]);
    if erange {
        return Err(Dterr::TzdispOverflow);
    }
    let mut cp = 1 + e;
    let mut min = 0;
    let mut sec = 0;
    if at(s, cp) == b':' {
        let (m, e, erange) = strtoint(&s[cp + 1..]);
        if erange {
            return Err(Dterr::TzdispOverflow);
        }
        min = m;
        cp += 1 + e;
        if at(s, cp) == b':' {
            let (sv, e, erange) = strtoint(&s[cp + 1..]);
            if erange {
                return Err(Dterr::TzdispOverflow);
            }
            sec = sv;
            cp += 1 + e;
        }
    } else if cp == s.len() && s.len() > 3 {
        min = hr % 100;
        hr /= 100;
    }
    if !(0..=MAX_TZDISP_HOUR).contains(&hr) || !(0..60).contains(&min) || !(0..60).contains(&sec) {
        return Err(Dterr::TzdispOverflow);
    }
    let mut tz = (hr * 60 + min) * 60 + sec;
    if sign == b'-' {
        tz = -tz;
    }
    if cp != s.len() {
        return Err(Dterr::BadFormat);
    }
    Ok(-tz)
}
// ─── DecodeDateTime / DecodeTimeOnly ────────────────────────────────────────

/// A decoded date/time value, enough for the input functions' range checks.
struct Decoded {
    dtype: i32,
    tm: Tm,
    fsec: i32,
    /// PG's `*tzp` (seconds west of UTC) when known exactly; `None` when it
    /// comes from a named zone, a dynamic abbreviation or the session zone.
    tz: Option<i32>,
}

/// `dt2time` for a fractional Julian day, in microseconds.
fn dt2time(jd: i64, tm: &mut Tm, fsec: &mut i32) {
    let mut time = jd;
    tm.hour = (time / USECS_PER_HOUR) as i32;
    time -= i64::from(tm.hour) * USECS_PER_HOUR;
    tm.min = (time / USECS_PER_MINUTE) as i32;
    time -= i64::from(tm.min) * USECS_PER_MINUTE;
    tm.sec = (time / USECS_PER_SEC) as i32;
    *fsec = (time - i64::from(tm.sec) * USECS_PER_SEC) as i32;
}

/// Record a `pg_tzset` answer for a zone field; `bad` is the error PG
/// raises when the zone doesn't exist.
fn apply_zone(lookup: ZoneLookup, bad: Dterr, unsure: &mut bool) -> DtResult<Option<bool>> {
    match lookup {
        ZoneLookup::Known { fixed } => Ok(Some(fixed)),
        ZoneLookup::NotFound => Err(bad),
        ZoneLookup::Unsure => {
            // PG either errors right here or carries on; any later error
            // of ours might therefore not be the one PG reports.
            *unsure = true;
            Ok(None)
        }
    }
}

/// `DecodeDateTime`: general date and time input (date, timestamp,
/// timestamptz). A time-only result (PG's return value 1) is reported as
/// `BadFormat`, which is how every caller treats it.
fn decode_date_time(fields: &[Field]) -> DtResult<Decoded> {
    let mut unsure = false;
    let r = decode_date_time_inner(fields, &mut unsure);
    match r {
        Err(_) if unsure => Err(Dterr::Unsure),
        r => r,
    }
}

fn decode_date_time_inner(fields: &[Field], unsure: &mut bool) -> DtResult<Decoded> {
    let mut fmask = 0i32;
    let mut tmask = 0i32;
    let mut ptype = 0i32;
    let mut mer = HR24;
    let mut have_text_month = false;
    let mut isjulian = false;
    let mut is2digits = false;
    let mut bc = false;
    let mut named_tz = false;
    let mut dyn_tz = false;
    let mut now_used = false;
    let mut tm = Tm::default();
    let mut fsec = 0i32;
    let mut tz: Option<i32> = Some(0);
    let mut dtype = DTK_DATE_V;

    for f in fields {
        let field = f.text.as_slice();
        match f.ftype {
            DTK_DATE => {
                if ptype == DTK_JULIAN {
                    // Integral julian day with attached time zone.
                    let (jday, end, erange) = strtoint(field);
                    if erange || jday < 0 {
                        return Err(Dterr::FieldOverflow);
                    }
                    (tm.year, tm.mon, tm.mday) = j2date(jday);
                    isjulian = true;
                    tz = Some(decode_timezone(&field[end..])?);
                    tmask = DTK_DATE_M | DTK_TIME_M | dtk_m(TZ);
                    ptype = 0;
                } else if ptype != 0
                    || (fmask & (dtk_m(MONTH) | dtk_m(DAY))) == (dtk_m(MONTH) | dtk_m(DAY))
                {
                    // A time zone name, or a run-together time with a
                    // trailing numeric zone (hhmmss-zz).
                    if field[0].is_ascii_digit() || ptype != 0 {
                        if ptype != 0 {
                            if ptype != DTK_TIME_V {
                                return Err(Dterr::BadFormat);
                            }
                            ptype = 0;
                        }
                        if (fmask & DTK_TIME_M) == DTK_TIME_M {
                            return Err(Dterr::BadFormat);
                        }
                        let Some(dash) = field.iter().position(|&c| c == b'-') else {
                            return Err(Dterr::BadFormat);
                        };
                        tz = Some(decode_timezone(&field[dash..])?);
                        decode_number_field(
                            &field[..dash],
                            fmask,
                            &mut tmask,
                            &mut tm,
                            &mut fsec,
                            &mut is2digits,
                        )?;
                        tmask |= dtk_m(TZ);
                    } else {
                        let bad = Dterr::BadTimezone(String::from_utf8_lossy(field).into_owned());
                        apply_zone(pg_tzset(field), bad, unsure)?;
                        named_tz = true;
                        tmask = dtk_m(TZ);
                    }
                } else {
                    decode_date(field, fmask, &mut tmask, &mut is2digits, &mut tm)?;
                }
            }
            DTK_TIME => {
                if ptype != 0 {
                    if ptype != DTK_TIME_V {
                        return Err(Dterr::BadFormat);
                    }
                    ptype = 0;
                }
                tmask = DTK_TIME_M;
                decode_time(field, &mut tm, &mut fsec)?;
                if time_overflows(tm.hour, tm.min, tm.sec, fsec) {
                    return Err(Dterr::FieldOverflow);
                }
            }
            DTK_TZ => {
                tz = Some(decode_timezone(field)?);
                tmask = dtk_m(TZ);
            }
            DTK_NUMBER => {
                if ptype != 0 {
                    let (value, end, erange) = strtoint(field);
                    if erange {
                        return Err(Dterr::FieldOverflow);
                    }
                    if end < field.len() && field[end] != b'.' {
                        return Err(Dterr::BadFormat);
                    }
                    match ptype {
                        DTK_JULIAN => {
                            if value < 0 {
                                return Err(Dterr::FieldOverflow);
                            }
                            tmask = DTK_DATE_M;
                            (tm.year, tm.mon, tm.mday) = j2date(value);
                            isjulian = true;
                            if end < field.len() {
                                let time = parse_fraction(&field[end..])? * USECS_PER_DAY as f64;
                                dt2time(time as i64, &mut tm, &mut fsec);
                                tmask |= DTK_TIME_M;
                            }
                        }
                        DTK_TIME_V => {
                            decode_number_field(
                                field,
                                fmask | DTK_DATE_M,
                                &mut tmask,
                                &mut tm,
                                &mut fsec,
                                &mut is2digits,
                            )?;
                            if tmask != DTK_TIME_M {
                                return Err(Dterr::BadFormat);
                            }
                        }
                        _ => return Err(Dterr::BadFormat),
                    }
                    ptype = 0;
                    dtype = DTK_DATE_V;
                } else {
                    let flen = field.len();
                    let dot = field.iter().position(|&c| c == b'.');
                    if dot.is_some() && fmask & DTK_DATE_M == 0 {
                        decode_date(field, fmask, &mut tmask, &mut is2digits, &mut tm)?;
                    } else if dot.is_some_and(|d| d > 2)
                        || (flen >= 6 && (fmask & DTK_DATE_M == 0 || fmask & DTK_TIME_M == 0))
                    {
                        // A concatenated date or time (20011223, 040506.5);
                        // six-plus digits are YMD/HMS unless both a date
                        // and a time are already present.
                        decode_number_field(
                            field,
                            fmask,
                            &mut tmask,
                            &mut tm,
                            &mut fsec,
                            &mut is2digits,
                        )?;
                    } else {
                        decode_number(
                            field,
                            have_text_month,
                            fmask,
                            &mut tmask,
                            &mut tm,
                            &mut fsec,
                            &mut is2digits,
                        )?;
                    }
                }
            }
            DTK_STRING | DTK_SPECIAL => {
                // Zone abbreviations take precedence over built-in tokens.
                let (ty, val, off) = match decode_timezone_abbrev(field) {
                    Some((ty, off)) => (ty, off.unwrap_or(0), off),
                    None => {
                        let (ty, val) = decode_special(field);
                        (ty, val, None)
                    }
                };
                if ty == IGNORE_DTF {
                    continue;
                }
                tmask = dtk_m(ty);
                match ty {
                    RESERV => match val {
                        DTK_NOW => {
                            tmask = DTK_DATE_M | DTK_TIME_M | dtk_m(TZ);
                            dtype = DTK_DATE_V;
                            now_used = true;
                            tm = Tm {
                                year: 2000,
                                mon: 1,
                                mday: 1,
                                hour: 0,
                                min: 0,
                                sec: 0,
                                yday: 0,
                            };
                            tz = None;
                        }
                        DTK_YESTERDAY | DTK_TODAY | DTK_TOMORROW => {
                            tmask = DTK_DATE_M;
                            dtype = DTK_DATE_V;
                            (tm.year, tm.mon, tm.mday) = (2000, 1, 2);
                        }
                        DTK_ZULU => {
                            tmask = DTK_TIME_M | dtk_m(TZ);
                            dtype = DTK_DATE_V;
                            (tm.hour, tm.min, tm.sec) = (0, 0, 0);
                            tz = Some(0);
                        }
                        _ => {
                            // DTK_EPOCH, DTK_LATE, DTK_EARLY
                            tmask = DTK_DATE_M | DTK_TIME_M | dtk_m(TZ);
                            dtype = val;
                        }
                    },
                    MONTH => {
                        if fmask & dtk_m(MONTH) != 0
                            && !have_text_month
                            && fmask & dtk_m(DAY) == 0
                            && (1..=31).contains(&tm.mon)
                        {
                            tm.mday = tm.mon;
                            tmask = dtk_m(DAY);
                        }
                        have_text_month = true;
                        tm.mon = val;
                    }
                    DTZMOD => {
                        tmask |= dtk_m(DTZ);
                        tz = tz.map(|t| t - val);
                    }
                    DTZ => {
                        tmask |= dtk_m(TZ);
                        tz = off.map(|o| -o);
                    }
                    TZ => tz = off.map(|o| -o),
                    DYNTZ => {
                        tmask |= dtk_m(TZ);
                        dyn_tz = true;
                    }
                    AMPM => mer = val,
                    ADBC => bc = val == BC,
                    DOW => {}
                    UNITS => {
                        tmask = 0;
                        if ptype != 0 {
                            return Err(Dterr::BadFormat);
                        }
                        ptype = val;
                    }
                    ISOTIME => {
                        tmask = 0;
                        if (fmask & DTK_DATE_M) != DTK_DATE_M || ptype != 0 {
                            return Err(Dterr::BadFormat);
                        }
                        ptype = val;
                    }
                    UNKNOWN_FIELD => {
                        // Perhaps an all-alpha time zone name.
                        apply_zone(pg_tzset(field), Dterr::BadFormat, unsure)?;
                        named_tz = true;
                        tmask = dtk_m(TZ);
                    }
                    _ => return Err(Dterr::BadFormat),
                }
            }
            _ => return Err(Dterr::BadFormat),
        }
        if tmask & fmask != 0 {
            return Err(Dterr::BadFormat);
        }
        fmask |= tmask;
    }

    if ptype != 0 {
        return Err(Dterr::BadFormat);
    }
    if dtype == DTK_DATE_V {
        validate_date(fmask, isjulian, is2digits, bc, &mut tm)?;
        if mer != HR24 && now_used {
            // Depends on the current time of day.
            return Err(Dterr::Unsure);
        }
        if mer != HR24 && tm.hour > 12 {
            return Err(Dterr::FieldOverflow);
        }
        if mer == AM && tm.hour == 12 {
            tm.hour = 0;
        } else if mer == PM && tm.hour != 12 {
            tm.hour += 12;
        }
        if (fmask & DTK_DATE_M) != DTK_DATE_M {
            return Err(Dterr::BadFormat);
        }
        if named_tz || dyn_tz {
            if fmask & dtk_m(DTZMOD) != 0 {
                return Err(Dterr::BadFormat);
            }
            tz = None;
        }
        if fmask & dtk_m(TZ) == 0 {
            if fmask & dtk_m(DTZMOD) != 0 {
                return Err(Dterr::BadFormat);
            }
            tz = None;
        }
    }
    Ok(Decoded {
        dtype,
        tm,
        fsec,
        tz,
    })
}

/// `DecodeTimeOnly`: time / timetz input.
fn decode_time_only(fields: &[Field]) -> DtResult<()> {
    let mut unsure = false;
    match decode_time_only_inner(fields, &mut unsure) {
        Err(_) if unsure => Err(Dterr::Unsure),
        r => r,
    }
}

fn decode_time_only_inner(fields: &[Field], unsure: &mut bool) -> DtResult<()> {
    let nf = fields.len();
    let mut fmask = 0i32;
    let mut tmask = 0i32;
    let mut ptype = 0i32;
    let mut isjulian = false;
    let mut is2digits = false;
    let mut bc = false;
    let mut mer = HR24;
    // `Some(fixed)` for a named zone (`None` fixedness = unsure).
    let mut named_tz: Option<Option<bool>> = None;
    let mut dyn_tz = false;
    let mut now_used = false;
    let mut tm = Tm::default();
    let mut fsec = 0i32;

    for (i, f) in fields.iter().enumerate() {
        let field = f.text.as_slice();
        match f.ftype {
            DTK_DATE => {
                if i == 0
                    && nf >= 2
                    && (fields[nf - 1].ftype == DTK_DATE || fields[1].ftype == DTK_TIME)
                {
                    decode_date(field, fmask, &mut tmask, &mut is2digits, &mut tm)?;
                } else if field[0].is_ascii_digit() {
                    if (fmask & DTK_TIME_M) == DTK_TIME_M {
                        return Err(Dterr::BadFormat);
                    }
                    let Some(dash) = field.iter().position(|&c| c == b'-') else {
                        return Err(Dterr::BadFormat);
                    };
                    decode_timezone(&field[dash..])?;
                    decode_number_field(
                        &field[..dash],
                        fmask | DTK_DATE_M,
                        &mut tmask,
                        &mut tm,
                        &mut fsec,
                        &mut is2digits,
                    )?;
                    tmask |= dtk_m(TZ);
                } else {
                    let bad = Dterr::BadTimezone(String::from_utf8_lossy(field).into_owned());
                    named_tz = Some(apply_zone(pg_tzset(field), bad, unsure)?);
                    tmask = dtk_m(TZ);
                }
            }
            DTK_TIME => {
                if ptype != 0 {
                    if ptype != DTK_TIME_V {
                        return Err(Dterr::BadFormat);
                    }
                    ptype = 0;
                }
                tmask = DTK_TIME_M;
                decode_time(field, &mut tm, &mut fsec)?;
            }
            DTK_TZ => {
                decode_timezone(field)?;
                tmask = dtk_m(TZ);
            }
            DTK_NUMBER => {
                if ptype != 0 {
                    let (value, end, erange) = strtoint(field);
                    if erange {
                        return Err(Dterr::FieldOverflow);
                    }
                    if end < field.len() && field[end] != b'.' {
                        return Err(Dterr::BadFormat);
                    }
                    match ptype {
                        DTK_JULIAN => {
                            if value < 0 {
                                return Err(Dterr::FieldOverflow);
                            }
                            tmask = DTK_DATE_M;
                            (tm.year, tm.mon, tm.mday) = j2date(value);
                            isjulian = true;
                            if end < field.len() {
                                let time = parse_fraction(&field[end..])? * USECS_PER_DAY as f64;
                                dt2time(time as i64, &mut tm, &mut fsec);
                                tmask |= DTK_TIME_M;
                            }
                        }
                        DTK_TIME_V => {
                            decode_number_field(
                                field,
                                fmask | DTK_DATE_M,
                                &mut tmask,
                                &mut tm,
                                &mut fsec,
                                &mut is2digits,
                            )?;
                            if tmask != DTK_TIME_M {
                                return Err(Dterr::BadFormat);
                            }
                        }
                        _ => return Err(Dterr::BadFormat),
                    }
                    ptype = 0;
                } else {
                    let flen = field.len();
                    if let Some(d) = field.iter().position(|&c| c == b'.') {
                        if i == 0 && nf >= 2 && fields[nf - 1].ftype == DTK_DATE {
                            decode_date(field, fmask, &mut tmask, &mut is2digits, &mut tm)?;
                        } else if d > 2 {
                            decode_number_field(
                                field,
                                fmask | DTK_DATE_M,
                                &mut tmask,
                                &mut tm,
                                &mut fsec,
                                &mut is2digits,
                            )?;
                        } else {
                            return Err(Dterr::BadFormat);
                        }
                    } else if flen > 4 {
                        decode_number_field(
                            field,
                            fmask | DTK_DATE_M,
                            &mut tmask,
                            &mut tm,
                            &mut fsec,
                            &mut is2digits,
                        )?;
                    } else {
                        decode_number(
                            field,
                            false,
                            fmask | DTK_DATE_M,
                            &mut tmask,
                            &mut tm,
                            &mut fsec,
                            &mut is2digits,
                        )?;
                    }
                }
            }
            DTK_STRING | DTK_SPECIAL => {
                let (ty, val) = match decode_timezone_abbrev(field) {
                    Some((ty, off)) => (ty, off.unwrap_or(0)),
                    None => decode_special(field),
                };
                if ty == IGNORE_DTF {
                    continue;
                }
                tmask = dtk_m(ty);
                match ty {
                    RESERV => match val {
                        DTK_NOW => {
                            tmask = DTK_TIME_M;
                            now_used = true;
                            (tm.hour, tm.min, tm.sec) = (0, 0, 0);
                        }
                        DTK_ZULU => {
                            tmask = DTK_TIME_M | dtk_m(TZ);
                            (tm.hour, tm.min, tm.sec) = (0, 0, 0);
                        }
                        _ => return Err(Dterr::BadFormat),
                    },
                    DTZMOD => tmask |= dtk_m(DTZ),
                    DTZ => tmask |= dtk_m(TZ),
                    TZ => {}
                    DYNTZ => {
                        tmask |= dtk_m(TZ);
                        dyn_tz = true;
                    }
                    AMPM => mer = val,
                    ADBC => bc = val == BC,
                    UNITS | ISOTIME => {
                        tmask = 0;
                        if ptype != 0 {
                            return Err(Dterr::BadFormat);
                        }
                        ptype = val;
                    }
                    UNKNOWN_FIELD => {
                        named_tz = Some(apply_zone(pg_tzset(field), Dterr::BadFormat, unsure)?);
                        tmask = dtk_m(TZ);
                    }
                    _ => return Err(Dterr::BadFormat),
                }
            }
            _ => return Err(Dterr::BadFormat),
        }
        if tmask & fmask != 0 {
            return Err(Dterr::BadFormat);
        }
        fmask |= tmask;
    }

    if ptype != 0 {
        return Err(Dterr::BadFormat);
    }
    validate_date(fmask, isjulian, is2digits, bc, &mut tm)?;
    if mer != HR24 && now_used {
        return Err(Dterr::Unsure);
    }
    if mer != HR24 && tm.hour > 12 {
        return Err(Dterr::FieldOverflow);
    }
    if mer == AM && tm.hour == 12 {
        tm.hour = 0;
    } else if mer == PM && tm.hour != 12 {
        tm.hour += 12;
    }
    if time_overflows(tm.hour, tm.min, tm.sec, fsec) {
        return Err(Dterr::FieldOverflow);
    }
    if (fmask & DTK_TIME_M) != DTK_TIME_M {
        return Err(Dterr::BadFormat);
    }
    let full_date = (fmask & DTK_DATE_M) == DTK_DATE_M;
    if let Some(fixed) = named_tz {
        if fmask & dtk_m(DTZMOD) != 0 {
            return Err(Dterr::BadFormat);
        }
        // A zone with more than one UTC offset needs a date to resolve.
        match fixed {
            Some(true) => {}
            Some(false) if !full_date => return Err(Dterr::BadFormat),
            Some(false) => {}
            None => return Err(Dterr::Unsure),
        }
    }
    if dyn_tz {
        if fmask & dtk_m(DTZMOD) != 0 {
            return Err(Dterr::BadFormat);
        }
        if fmask & DTK_DATE_M != 0 && !full_date {
            return Err(Dterr::BadFormat);
        }
    }
    if fmask & dtk_m(TZ) == 0 {
        if fmask & dtk_m(DTZMOD) != 0 {
            return Err(Dterr::BadFormat);
        }
        if fmask & DTK_DATE_M != 0 && !full_date {
            return Err(Dterr::BadFormat);
        }
    }
    Ok(())
}
// ─── input functions ────────────────────────────────────────────────────────

/// `date_in` (date.c).
fn date_in(content: &str) -> DtResult<()> {
    let fields = parse_date_time(content.as_bytes(), MAXDATELEN + 1)?;
    let d = decode_date_time(&fields)?;
    if d.dtype != DTK_DATE_V {
        // epoch / ±infinity
        return Ok(());
    }
    let tm = d.tm;
    if !is_valid_julian(tm.year, tm.mon) {
        return Err(Dterr::DateOutOfRange);
    }
    let date = i64::from(date2j(tm.year, tm.mon, tm.mday)) - POSTGRES_EPOCH_JDATE;
    if !(DATETIME_MIN_JULIAN - POSTGRES_EPOCH_JDATE..DATE_END_JULIAN - POSTGRES_EPOCH_JDATE)
        .contains(&date)
    {
        return Err(Dterr::DateOutOfRange);
    }
    Ok(())
}

/// `time_in` / `timetz_in` (date.c): no range check beyond the decoder's.
fn time_in(content: &str) -> DtResult<()> {
    let fields = parse_date_time(content.as_bytes(), MAXDATELEN + 1)?;
    decode_time_only(&fields)
}

/// `timestamp_in` / `timestamptz_in` (timestamp.c) with `tm2timestamp`'s
/// range check.
fn timestamp_in(content: &str, with_tz: bool) -> DtResult<()> {
    let fields = parse_date_time(content.as_bytes(), MAXDATELEN + MAXDATEFIELDS)?;
    let d = decode_date_time(&fields)?;
    if d.dtype != DTK_DATE_V {
        return Ok(());
    }
    let tm = d.tm;
    if !is_valid_julian(tm.year, tm.mon) {
        return Err(Dterr::TimestampOutOfRange);
    }
    let date = i64::from(date2j(tm.year, tm.mon, tm.mday)) - POSTGRES_EPOCH_JDATE;
    let time = ((i64::from(tm.hour) * 60 + i64::from(tm.min)) * 60 + i64::from(tm.sec))
        * USECS_PER_SEC
        + i64::from(d.fsec);
    let Some(local) = date
        .checked_mul(USECS_PER_DAY)
        .and_then(|v| v.checked_add(time))
    else {
        return Err(Dterr::TimestampOutOfRange);
    };
    let valid = |t: i64| (MIN_TIMESTAMP..END_TIMESTAMP).contains(&t);
    if !with_tz {
        return if valid(local) {
            Ok(())
        } else {
            Err(Dterr::TimestampOutOfRange)
        };
    }
    match d.tz {
        Some(tz) => {
            // dt2local(result, -tz)
            if valid(local.wrapping_add(i64::from(tz) * USECS_PER_SEC)) {
                Ok(())
            } else {
                Err(Dterr::TimestampOutOfRange)
            }
        }
        // Unknown offset (at most a day either way): only decidable away
        // from the range limits.
        None if (MIN_TIMESTAMP + USECS_PER_DAY..END_TIMESTAMP - USECS_PER_DAY).contains(&local) => {
            Ok(())
        }
        None if !(MIN_TIMESTAMP - USECS_PER_DAY..END_TIMESTAMP + USECS_PER_DAY)
            .contains(&local) =>
        {
            Err(Dterr::TimestampOutOfRange)
        }
        None => Err(Dterr::Unsure),
    }
}

/// `interval_in` (timestamp.c). The typmod's field restriction changes the
/// decoding; not knowing it, reject only when every restriction rejects
/// with the same error.
fn interval_in(content: &str) -> DtResult<()> {
    const RANGES: [i32; 7] = [
        INTERVAL_FULL_RANGE,
        interval_mask(YEAR),
        interval_mask(MONTH),
        interval_mask(DAY),
        interval_mask(HOUR),
        interval_mask(MINUTE),
        MINUTE_TO_SECOND,
    ];
    let first = interval_in_range(content, INTERVAL_FULL_RANGE);
    let Err(err) = first else {
        return Ok(());
    };
    for range in &RANGES[1..] {
        if interval_in_range(content, *range).as_ref() != Err(&err) {
            return Err(Dterr::Unsure);
        }
    }
    Err(err)
}

fn interval_in_range(content: &str, range: i32) -> DtResult<()> {
    let mut itm = ItmIn::default();
    let mut dtype = DTK_DELTA;
    let mut r = parse_date_time(content.as_bytes(), 256)
        .and_then(|fields| decode_interval(&fields, range, &mut dtype, &mut itm));
    // If those think it's a bad format, try ISO 8601 style.
    if r == Err(Dterr::BadFormat) {
        itm = ItmIn::default();
        dtype = DTK_DELTA;
        r = decode_iso8601_interval(content.as_bytes(), &mut itm);
    }
    match r {
        Err(Dterr::FieldOverflow) => return Err(Dterr::IntervalOverflow),
        Err(e) => return Err(e),
        Ok(()) => {}
    }
    if dtype == DTK_DELTA {
        // itmin2interval
        let months = i64::from(itm.year) * i64::from(MONTHS_PER_YEAR) + i64::from(itm.mon);
        if months > i64::from(i32::MAX) || months < i64::from(i32::MIN) {
            return Err(Dterr::IntervalOutOfRange);
        }
    }
    Ok(())
}

// ─── DecodeInterval / DecodeISO8601Interval ─────────────────────────────────

/// `struct pg_itm_in`.
#[derive(Default)]
struct ItmIn {
    usec: i64,
    mday: i32,
    mon: i32,
    year: i32,
}

/// `AdjustFractMicroseconds`.
fn adjust_fract_microseconds(frac: f64, scale: i64, itm: &mut ItmIn) -> bool {
    if frac == 0.0 {
        return true;
    }
    let mut frac = frac * scale as f64;
    let mut usec = frac as i64;
    frac -= usec as f64;
    if frac > 0.5 {
        usec += 1;
    } else if frac < -0.5 {
        usec -= 1;
    }
    match itm.usec.checked_add(usec) {
        Some(v) => {
            itm.usec = v;
            true
        }
        None => false,
    }
}

/// `AdjustFractDays`.
fn adjust_fract_days(frac: f64, scale: i32, itm: &mut ItmIn) -> bool {
    if frac == 0.0 {
        return true;
    }
    let frac = frac * f64::from(scale);
    let extra_days = frac as i32;
    match itm.mday.checked_add(extra_days) {
        Some(v) => itm.mday = v,
        None => return false,
    }
    adjust_fract_microseconds(frac - f64::from(extra_days), USECS_PER_DAY, itm)
}

/// `AdjustFractYears`.
fn adjust_fract_years(frac: f64, scale: i32, itm: &mut ItmIn) -> bool {
    let extra_months =
        (frac * f64::from(scale) * f64::from(MONTHS_PER_YEAR)).round_ties_even() as i32;
    match itm.mon.checked_add(extra_months) {
        Some(v) => {
            itm.mon = v;
            true
        }
        None => false,
    }
}

/// `AdjustMicroseconds`.
fn adjust_microseconds(val: i64, fval: f64, scale: i64, itm: &mut ItmIn) -> bool {
    match val.checked_mul(scale).and_then(|p| itm.usec.checked_add(p)) {
        Some(v) => itm.usec = v,
        None => return false,
    }
    adjust_fract_microseconds(fval, scale, itm)
}

/// `AdjustDays`.
fn adjust_days(val: i64, scale: i32, itm: &mut ItmIn) -> bool {
    let Ok(val) = i32::try_from(val) else {
        return false;
    };
    match val.checked_mul(scale).and_then(|d| itm.mday.checked_add(d)) {
        Some(v) => {
            itm.mday = v;
            true
        }
        None => false,
    }
}

/// `AdjustMonths`.
fn adjust_months(val: i64, itm: &mut ItmIn) -> bool {
    match i32::try_from(val).ok().and_then(|v| itm.mon.checked_add(v)) {
        Some(v) => {
            itm.mon = v;
            true
        }
        None => false,
    }
}

/// `AdjustYears`.
fn adjust_years(val: i64, scale: i32, itm: &mut ItmIn) -> bool {
    match i32::try_from(val)
        .ok()
        .and_then(|v| v.checked_mul(scale))
        .and_then(|y| itm.year.checked_add(y))
    {
        Some(v) => {
            itm.year = v;
            true
        }
        None => false,
    }
}

/// `DecodeTimeForInterval`: note it *overwrites* the accumulated
/// microseconds, as PG does.
fn decode_time_for_interval(s: &[u8], range: i32, itm: &mut ItmIn) -> DtResult<()> {
    let t = decode_time_common(s, range)?;
    itm.usec = i64::from(t.usec);
    let ok = t
        .hour
        .checked_mul(USECS_PER_HOUR)
        .and_then(|v| itm.usec.checked_add(v))
        .and_then(|v| v.checked_add(i64::from(t.min) * USECS_PER_MINUTE))
        .and_then(|v| v.checked_add(i64::from(t.sec) * USECS_PER_SEC));
    match ok {
        Some(v) => {
            itm.usec = v;
            Ok(())
        }
        None => Err(Dterr::FieldOverflow),
    }
}

/// `DecodeInterval` (IntervalStyle = postgres).
fn decode_interval(fields: &[Field], range: i32, dtype: &mut i32, itm: &mut ItmIn) -> DtResult<()> {
    let nf = fields.len();
    let mut is_before = false;
    let mut parsing_unit_val = false;
    let mut fmask = 0i32;
    let mut ty = IGNORE_DTF;
    *dtype = DTK_DELTA;

    // Read backwards to pick up units before values.
    for i in (0..nf).rev() {
        let field = fields[i].text.as_slice();
        let tmask;
        let mut ftype = fields[i].ftype;
        if ftype == DTK_TIME {
            decode_time_for_interval(field, range, itm)?;
            tmask = DTK_TIME_M;
            ty = DTK_DAY;
            parsing_unit_val = false;
            if tmask & fmask != 0 {
                return Err(Dterr::BadFormat);
            }
            fmask |= tmask;
            continue;
        }
        if ftype == DTK_TZ {
            // Signed hh:mm[:ss]?
            if field[1..].contains(&b':') {
                let mut probe = ItmIn {
                    usec: itm.usec,
                    ..ItmIn::default()
                };
                if decode_time_for_interval(&field[1..], range, &mut probe).is_ok() {
                    itm.usec = probe.usec;
                    if field[0] == b'-' {
                        if itm.usec == i64::MIN {
                            return Err(Dterr::FieldOverflow);
                        }
                        itm.usec = -itm.usec;
                    }
                    ty = DTK_DAY;
                    parsing_unit_val = false;
                    tmask = DTK_TIME_M;
                    if tmask & fmask != 0 {
                        return Err(Dterr::BadFormat);
                    }
                    fmask |= tmask;
                    continue;
                }
            }
            // Otherwise a signed number / year-month value.
            ftype = DTK_NUMBER;
        }
        match ftype {
            DTK_DATE | DTK_NUMBER => {
                if ty == IGNORE_DTF {
                    // The typmod decides what the rightmost field is.
                    ty = match range {
                        r if r == interval_mask(YEAR) => DTK_YEAR,
                        r if r == interval_mask(MONTH)
                            || r == interval_mask(YEAR) | interval_mask(MONTH) =>
                        {
                            DTK_MONTH
                        }
                        r if r == interval_mask(DAY) => DTK_DAY,
                        r if r == interval_mask(HOUR)
                            || r == interval_mask(DAY) | interval_mask(HOUR) =>
                        {
                            DTK_HOUR
                        }
                        r if r == interval_mask(MINUTE)
                            || r == interval_mask(HOUR) | interval_mask(MINUTE)
                            || r == interval_mask(DAY)
                                | interval_mask(HOUR)
                                | interval_mask(MINUTE) =>
                        {
                            DTK_MINUTE
                        }
                        _ => DTK_SECOND,
                    };
                }
                let (mut val, end, erange) = strtol(field);
                if erange {
                    return Err(Dterr::FieldOverflow);
                }
                let fval;
                match at(field, end) {
                    b'-' => {
                        // SQL "years-months" syntax.
                        let (mut val2, e2, erange2) = strtoint(&field[end + 1..]);
                        if erange2 || !(0..MONTHS_PER_YEAR).contains(&val2) {
                            return Err(Dterr::FieldOverflow);
                        }
                        if end + 1 + e2 != field.len() {
                            return Err(Dterr::BadFormat);
                        }
                        ty = DTK_MONTH;
                        if field[0] == b'-' {
                            val2 = -val2;
                        }
                        val = val
                            .checked_mul(i64::from(MONTHS_PER_YEAR))
                            .and_then(|v| v.checked_add(i64::from(val2)))
                            .ok_or(Dterr::FieldOverflow)?;
                        fval = 0.0;
                    }
                    b'.' => {
                        let f = parse_fraction(&field[end..])?;
                        fval = if field[0] == b'-' { -f } else { f };
                    }
                    0 if end == field.len() => fval = 0.0,
                    _ => return Err(Dterr::BadFormat),
                }
                let ok;
                match ty {
                    DTK_MICROSEC => {
                        ok = adjust_microseconds(val, fval, 1, itm);
                        tmask = dtk_m(MICROSECOND);
                    }
                    DTK_MILLISEC => {
                        ok = adjust_microseconds(val, fval, 1000, itm);
                        tmask = dtk_m(MILLISECOND);
                    }
                    DTK_SECOND => {
                        ok = adjust_microseconds(val, fval, USECS_PER_SEC, itm);
                        tmask = if fval == 0.0 {
                            dtk_m(SECOND)
                        } else {
                            DTK_ALL_SECS_M
                        };
                    }
                    DTK_MINUTE => {
                        ok = adjust_microseconds(val, fval, USECS_PER_MINUTE, itm);
                        tmask = dtk_m(MINUTE);
                    }
                    DTK_HOUR => {
                        ok = adjust_microseconds(val, fval, USECS_PER_HOUR, itm);
                        tmask = dtk_m(HOUR);
                        ty = DTK_DAY;
                    }
                    DTK_DAY => {
                        ok = adjust_days(val, 1, itm)
                            && adjust_fract_microseconds(fval, USECS_PER_DAY, itm);
                        tmask = dtk_m(DAY);
                    }
                    DTK_WEEK => {
                        ok = adjust_days(val, 7, itm) && adjust_fract_days(fval, 7, itm);
                        tmask = dtk_m(WEEK);
                    }
                    DTK_MONTH => {
                        ok =
                            adjust_months(val, itm) && adjust_fract_days(fval, DAYS_PER_MONTH, itm);
                        tmask = dtk_m(MONTH);
                    }
                    DTK_YEAR => {
                        ok = adjust_years(val, 1, itm) && adjust_fract_years(fval, 1, itm);
                        tmask = dtk_m(YEAR);
                    }
                    DTK_DECADE => {
                        ok = adjust_years(val, 10, itm) && adjust_fract_years(fval, 10, itm);
                        tmask = dtk_m(DECADE);
                    }
                    DTK_CENTURY => {
                        ok = adjust_years(val, 100, itm) && adjust_fract_years(fval, 100, itm);
                        tmask = dtk_m(CENTURY);
                    }
                    DTK_MILLENNIUM => {
                        ok = adjust_years(val, 1000, itm) && adjust_fract_years(fval, 1000, itm);
                        tmask = dtk_m(MILLENNIUM);
                    }
                    _ => return Err(Dterr::BadFormat),
                }
                if !ok {
                    return Err(Dterr::FieldOverflow);
                }
                parsing_unit_val = false;
            }
            DTK_STRING | DTK_SPECIAL => {
                if parsing_unit_val {
                    return Err(Dterr::BadFormat);
                }
                let (mut t, mut uval) = decode_units(field);
                if t == UNKNOWN_FIELD {
                    (t, uval) = decode_special(field);
                }
                ty = t;
                if t == IGNORE_DTF {
                    continue;
                }
                match t {
                    UNITS => {
                        ty = uval;
                        parsing_unit_val = true;
                        tmask = 0;
                    }
                    AGO => {
                        // Only allowed at the end.
                        if i != nf - 1 {
                            return Err(Dterr::BadFormat);
                        }
                        is_before = true;
                        ty = uval;
                        tmask = 0;
                    }
                    RESERV => {
                        tmask = DTK_DATE_M | DTK_TIME_M;
                        // Only infinities, and nothing after them.
                        if (uval != DTK_LATE && uval != DTK_EARLY) || i != nf - 1 {
                            return Err(Dterr::BadFormat);
                        }
                        *dtype = uval;
                    }
                    _ => return Err(Dterr::BadFormat),
                }
            }
            _ => return Err(Dterr::BadFormat),
        }
        if tmask & fmask != 0 {
            return Err(Dterr::BadFormat);
        }
        fmask |= tmask;
    }

    if fmask == 0 || parsing_unit_val {
        return Err(Dterr::BadFormat);
    }
    if is_before
        && (itm.usec == i64::MIN
            || itm.mday == i32::MIN
            || itm.mon == i32::MIN
            || itm.year == i32::MIN)
    {
        return Err(Dterr::FieldOverflow);
    }
    Ok(())
}

/// glibc `strtod` prefix parse: `(value, consumed, erange)`, `None` when
/// nothing converts. Hexadecimal floats are `Unsure`.
fn strtod(s: &[u8]) -> DtResult<Option<(f64, usize, bool)>> {
    let mut i = 0;
    while i < s.len() && is_space(s[i]) {
        i += 1;
    }
    let neg = match at(s, i) {
        b'-' => {
            i += 1;
            true
        }
        b'+' => {
            i += 1;
            false
        }
        _ => false,
    };
    let rest = &s[i..];
    let lower: Vec<u8> = rest.iter().take(8).map(u8::to_ascii_lowercase).collect();
    let sign = if neg { -1.0 } else { 1.0 };
    if lower.starts_with(b"infinity") {
        return Ok(Some((sign * f64::INFINITY, i + 8, false)));
    }
    if lower.starts_with(b"inf") {
        return Ok(Some((sign * f64::INFINITY, i + 3, false)));
    }
    if lower.starts_with(b"nan") {
        let mut j = i + 3;
        if at(s, j) == b'(' {
            let mut k = j + 1;
            while at(s, k).is_ascii_alphanumeric() || at(s, k) == b'_' {
                k += 1;
            }
            if at(s, k) == b')' {
                j = k + 1;
            }
        }
        return Ok(Some((f64::NAN, j, false)));
    }
    if lower.len() >= 3
        && lower[0] == b'0'
        && lower[1] == b'x'
        && (lower[2].is_ascii_hexdigit() || (lower[2] == b'.' && at(rest, 3).is_ascii_hexdigit()))
    {
        return Err(Dterr::Unsure);
    }
    let start = i;
    let mut digits = 0;
    let mut nonzero = false;
    while at(s, i).is_ascii_digit() {
        nonzero |= s[i] != b'0';
        digits += 1;
        i += 1;
    }
    if at(s, i) == b'.' {
        i += 1;
        while at(s, i).is_ascii_digit() {
            nonzero |= s[i] != b'0';
            digits += 1;
            i += 1;
        }
    }
    if digits == 0 {
        return Ok(None);
    }
    if matches!(at(s, i), b'e' | b'E') {
        let mut j = i + 1;
        if matches!(at(s, j), b'+' | b'-') {
            j += 1;
        }
        if at(s, j).is_ascii_digit() {
            while at(s, j).is_ascii_digit() {
                j += 1;
            }
            i = j;
        }
    }
    let text = String::from_utf8_lossy(&s[start..i]);
    let v: f64 = text.parse().map_err(|_| Dterr::Unsure)?;
    // glibc reports ERANGE on overflow and on underflow to zero or a
    // subnormal (verified: `'P1e-310D'::interval` is rejected).
    let erange = v.is_infinite() || (nonzero && !v.is_normal());
    Ok(Some((sign * v, i, erange)))
}

/// `ParseISO8601Number`: `(ipart, fpart, end)`.
fn parse_iso8601_number(s: &[u8], i: usize) -> DtResult<(i64, f64, usize)> {
    let c = at(s, i);
    if !(c.is_ascii_digit() || c == b'-' || c == b'.') {
        return Err(Dterr::BadFormat);
    }
    let Some((val, used, erange)) = strtod(&s[i..])? else {
        return Err(Dterr::BadFormat);
    };
    if erange {
        return Err(Dterr::BadFormat);
    }
    if val.is_nan() || !(-1.0e15..=1.0e15).contains(&val) {
        return Err(Dterr::FieldOverflow);
    }
    let ipart = if val >= 0.0 {
        val.floor() as i64
    } else {
        -((-val).floor() as i64)
    };
    Ok((ipart, val - ipart as f64, i + used))
}

/// `ISO8601IntegerWidth`.
fn iso8601_integer_width(s: &[u8], mut i: usize) -> usize {
    if at(s, i) == b'-' {
        i += 1;
    }
    s[i.min(s.len())..]
        .iter()
        .take_while(|c| c.is_ascii_digit())
        .count()
}

/// `DecodeISO8601Interval`: `P…` "format with designators" and the
/// "alternative format", on the raw (case-sensitive) input.
fn decode_iso8601_interval(s: &[u8], itm: &mut ItmIn) -> DtResult<()> {
    let ovf = |ok: bool| {
        if ok {
            Ok(())
        } else {
            Err(Dterr::FieldOverflow)
        }
    };
    if s.len() < 2 || s[0] != b'P' {
        return Err(Dterr::BadFormat);
    }
    let mut datepart = true;
    let mut havefield = false;
    let mut i = 1;
    while i < s.len() {
        if s[i] == b'T' {
            datepart = false;
            havefield = false;
            i += 1;
            continue;
        }
        let fieldstart = i;
        let (val, fval, end) = parse_iso8601_number(s, i)?;
        i = end;
        let unit = at(s, i);
        i += 1;
        if datepart {
            match unit {
                b'Y' => ovf(adjust_years(val, 1, itm) && adjust_fract_years(fval, 1, itm))?,
                b'M' => {
                    ovf(adjust_months(val, itm) && adjust_fract_days(fval, DAYS_PER_MONTH, itm))?
                }
                b'W' => ovf(adjust_days(val, 7, itm) && adjust_fract_days(fval, 7, itm))?,
                b'D' => {
                    ovf(adjust_days(val, 1, itm)
                        && adjust_fract_microseconds(fval, USECS_PER_DAY, itm))?
                }
                b'T' | 0 | b'-' => {
                    if unit != b'-' && iso8601_integer_width(s, fieldstart) == 8 && !havefield {
                        ovf(adjust_years(val / 10000, 1, itm)
                            && adjust_months((val / 100) % 100, itm)
                            && adjust_days(val % 100, 1, itm)
                            && adjust_fract_microseconds(fval, USECS_PER_DAY, itm))?;
                        if unit == 0 {
                            return Ok(());
                        }
                        datepart = false;
                        havefield = false;
                        continue;
                    }
                    // Extended alternative format.
                    if havefield {
                        return Err(Dterr::BadFormat);
                    }
                    ovf(adjust_years(val, 1, itm) && adjust_fract_years(fval, 1, itm))?;
                    if unit == 0 {
                        return Ok(());
                    }
                    if unit == b'T' {
                        datepart = false;
                        havefield = false;
                        continue;
                    }
                    let (val, fval, end) = parse_iso8601_number(s, i)?;
                    i = end;
                    ovf(adjust_months(val, itm) && adjust_fract_days(fval, DAYS_PER_MONTH, itm))?;
                    match at(s, i) {
                        0 => return Ok(()),
                        b'T' => {
                            datepart = false;
                            havefield = false;
                            i += 1;
                            continue;
                        }
                        b'-' => i += 1,
                        _ => return Err(Dterr::BadFormat),
                    }
                    let (val, fval, end) = parse_iso8601_number(s, i)?;
                    i = end;
                    ovf(adjust_days(val, 1, itm)
                        && adjust_fract_microseconds(fval, USECS_PER_DAY, itm))?;
                    match at(s, i) {
                        0 => return Ok(()),
                        b'T' => {
                            datepart = false;
                            havefield = false;
                            i += 1;
                            continue;
                        }
                        _ => return Err(Dterr::BadFormat),
                    }
                }
                _ => return Err(Dterr::BadFormat),
            }
        } else {
            match unit {
                b'H' => ovf(adjust_microseconds(val, fval, USECS_PER_HOUR, itm))?,
                b'M' => ovf(adjust_microseconds(val, fval, USECS_PER_MINUTE, itm))?,
                b'S' => ovf(adjust_microseconds(val, fval, USECS_PER_SEC, itm))?,
                0 | b':' => {
                    if unit == 0 && iso8601_integer_width(s, fieldstart) == 6 && !havefield {
                        ovf(adjust_microseconds(val / 10000, 0.0, USECS_PER_HOUR, itm)
                            && adjust_microseconds((val / 100) % 100, 0.0, USECS_PER_MINUTE, itm)
                            && adjust_microseconds(val % 100, 0.0, USECS_PER_SEC, itm)
                            && adjust_fract_microseconds(fval, 1, itm))?;
                        return Ok(());
                    }
                    if havefield {
                        return Err(Dterr::BadFormat);
                    }
                    ovf(adjust_microseconds(val, fval, USECS_PER_HOUR, itm))?;
                    if unit == 0 {
                        return Ok(());
                    }
                    let (val, fval, end) = parse_iso8601_number(s, i)?;
                    i = end;
                    ovf(adjust_microseconds(val, fval, USECS_PER_MINUTE, itm))?;
                    match at(s, i) {
                        0 => return Ok(()),
                        b':' => i += 1,
                        _ => return Err(Dterr::BadFormat),
                    }
                    let (val, fval, end) = parse_iso8601_number(s, i)?;
                    i = end;
                    ovf(adjust_microseconds(val, fval, USECS_PER_SEC, itm))?;
                    if i >= s.len() {
                        return Ok(());
                    }
                    return Err(Dterr::BadFormat);
                }
                _ => return Err(Dterr::BadFormat),
            }
        }
        havefield = true;
    }
    Ok(())
}
// ─── keyword tables ─────────────────────────────────────────────────────────

/// `datetktbl` (datetime.c): date/time keywords.
const DATETKTBL: &[(&str, i32, i32)] = &[
    ("+infinity", RESERV, DTK_LATE),
    ("-infinity", RESERV, DTK_EARLY),
    ("ad", ADBC, AD),
    ("allballs", RESERV, DTK_ZULU),
    ("am", AMPM, AM),
    ("apr", MONTH, 4),
    ("april", MONTH, 4),
    ("at", IGNORE_DTF, 0),
    ("aug", MONTH, 8),
    ("august", MONTH, 8),
    ("bc", ADBC, BC),
    ("d", UNITS, DTK_DAY),
    ("dec", MONTH, 12),
    ("december", MONTH, 12),
    ("dow", UNITS, DTK_DOW),
    ("doy", UNITS, DTK_DOY),
    ("dst", DTZMOD, 3600),
    ("epoch", RESERV, DTK_EPOCH),
    ("feb", MONTH, 2),
    ("february", MONTH, 2),
    ("fri", DOW, 5),
    ("friday", DOW, 5),
    ("h", UNITS, DTK_HOUR),
    ("infinity", RESERV, DTK_LATE),
    ("isodow", UNITS, DTK_ISODOW),
    ("isoyear", UNITS, DTK_ISOYEAR),
    ("j", UNITS, DTK_JULIAN),
    ("jan", MONTH, 1),
    ("january", MONTH, 1),
    ("jd", UNITS, DTK_JULIAN),
    ("jul", MONTH, 7),
    ("julian", UNITS, DTK_JULIAN),
    ("july", MONTH, 7),
    ("jun", MONTH, 6),
    ("june", MONTH, 6),
    ("m", UNITS, DTK_MONTH),
    ("mar", MONTH, 3),
    ("march", MONTH, 3),
    ("may", MONTH, 5),
    ("mm", UNITS, DTK_MINUTE),
    ("mon", DOW, 1),
    ("monday", DOW, 1),
    ("nov", MONTH, 11),
    ("november", MONTH, 11),
    ("now", RESERV, DTK_NOW),
    ("oct", MONTH, 10),
    ("october", MONTH, 10),
    ("on", IGNORE_DTF, 0),
    ("pm", AMPM, PM),
    ("s", UNITS, DTK_SECOND),
    ("sat", DOW, 6),
    ("saturday", DOW, 6),
    ("sep", MONTH, 9),
    ("sept", MONTH, 9),
    ("september", MONTH, 9),
    ("sun", DOW, 0),
    ("sunday", DOW, 0),
    ("t", ISOTIME, DTK_TIME_V),
    ("thu", DOW, 4),
    ("thur", DOW, 4),
    ("thurs", DOW, 4),
    ("thursday", DOW, 4),
    ("today", RESERV, DTK_TODAY),
    ("tomorrow", RESERV, DTK_TOMORROW),
    ("tue", DOW, 2),
    ("tues", DOW, 2),
    ("tuesday", DOW, 2),
    ("wed", DOW, 3),
    ("wednesday", DOW, 3),
    ("weds", DOW, 3),
    ("y", UNITS, DTK_YEAR),
    ("yesterday", RESERV, DTK_YESTERDAY),
];

/// `deltatktbl` (datetime.c): interval units. Entries longer than
/// `TOKMAXLEN` are stored truncated, as in PG.
const DELTATKTBL: &[(&str, i32, i32)] = &[
    ("@", IGNORE_DTF, 0),
    ("ago", AGO, 0),
    ("c", UNITS, DTK_CENTURY),
    ("cent", UNITS, DTK_CENTURY),
    ("centuries", UNITS, DTK_CENTURY),
    ("century", UNITS, DTK_CENTURY),
    ("d", UNITS, DTK_DAY),
    ("day", UNITS, DTK_DAY),
    ("days", UNITS, DTK_DAY),
    ("dec", UNITS, DTK_DECADE),
    ("decade", UNITS, DTK_DECADE),
    ("decades", UNITS, DTK_DECADE),
    ("decs", UNITS, DTK_DECADE),
    ("h", UNITS, DTK_HOUR),
    ("hour", UNITS, DTK_HOUR),
    ("hours", UNITS, DTK_HOUR),
    ("hr", UNITS, DTK_HOUR),
    ("hrs", UNITS, DTK_HOUR),
    ("m", UNITS, DTK_MINUTE),
    ("microsecon", UNITS, DTK_MICROSEC),
    ("mil", UNITS, DTK_MILLENNIUM),
    ("millennia", UNITS, DTK_MILLENNIUM),
    ("millennium", UNITS, DTK_MILLENNIUM),
    ("millisecon", UNITS, DTK_MILLISEC),
    ("mils", UNITS, DTK_MILLENNIUM),
    ("min", UNITS, DTK_MINUTE),
    ("mins", UNITS, DTK_MINUTE),
    ("minute", UNITS, DTK_MINUTE),
    ("minutes", UNITS, DTK_MINUTE),
    ("mon", UNITS, DTK_MONTH),
    ("mons", UNITS, DTK_MONTH),
    ("month", UNITS, DTK_MONTH),
    ("months", UNITS, DTK_MONTH),
    ("ms", UNITS, DTK_MILLISEC),
    ("msec", UNITS, DTK_MILLISEC),
    ("msecond", UNITS, DTK_MILLISEC),
    ("mseconds", UNITS, DTK_MILLISEC),
    ("msecs", UNITS, DTK_MILLISEC),
    ("qtr", UNITS, DTK_QUARTER),
    ("quarter", UNITS, DTK_QUARTER),
    ("s", UNITS, DTK_SECOND),
    ("sec", UNITS, DTK_SECOND),
    ("second", UNITS, DTK_SECOND),
    ("seconds", UNITS, DTK_SECOND),
    ("secs", UNITS, DTK_SECOND),
    ("timezone", UNITS, DTK_TZ_V),
    ("timezone_h", UNITS, DTK_TZ_HOUR),
    ("timezone_m", UNITS, DTK_TZ_MINUTE),
    ("us", UNITS, DTK_MICROSEC),
    ("usec", UNITS, DTK_MICROSEC),
    ("usecond", UNITS, DTK_MICROSEC),
    ("useconds", UNITS, DTK_MICROSEC),
    ("usecs", UNITS, DTK_MICROSEC),
    ("w", UNITS, DTK_WEEK),
    ("week", UNITS, DTK_WEEK),
    ("weeks", UNITS, DTK_WEEK),
    ("y", UNITS, DTK_YEAR),
    ("year", UNITS, DTK_YEAR),
    ("years", UNITS, DTK_YEAR),
    ("yr", UNITS, DTK_YEAR),
    ("yrs", UNITS, DTK_YEAR),
];

/// The `Default` timezone_abbreviations set
/// (`share/timezonesets/Default`, PG 18), lowercased; offsets in seconds
/// east of UTC.
const ZONE_ABBREVS: &[(&str, Abbrev)] = &[
    ("acdt", Abbrev::Dtz(37800)),
    ("acsst", Abbrev::Dtz(37800)),
    ("acst", Abbrev::Tz(34200)),
    ("act", Abbrev::Tz(-18000)),
    ("acwst", Abbrev::Tz(31500)),
    ("adt", Abbrev::Dtz(-10800)),
    ("aedt", Abbrev::Dtz(39600)),
    ("aesst", Abbrev::Dtz(39600)),
    ("aest", Abbrev::Tz(36000)),
    ("aft", Abbrev::Tz(16200)),
    ("akdt", Abbrev::Dtz(-28800)),
    ("akst", Abbrev::Tz(-32400)),
    ("almst", Abbrev::Dtz(25200)),
    ("almt", Abbrev::Tz(21600)),
    ("amst", Abbrev::Dyn),
    ("amt", Abbrev::Tz(-14400)),
    ("anast", Abbrev::Dyn),
    ("anat", Abbrev::Dyn),
    ("arst", Abbrev::Dyn),
    ("art", Abbrev::Dyn),
    ("ast", Abbrev::Tz(-14400)),
    ("awsst", Abbrev::Dtz(32400)),
    ("awst", Abbrev::Tz(28800)),
    ("azost", Abbrev::Dtz(0)),
    ("azot", Abbrev::Tz(-3600)),
    ("azst", Abbrev::Dyn),
    ("azt", Abbrev::Dyn),
    ("bdst", Abbrev::Dtz(7200)),
    ("bdt", Abbrev::Tz(21600)),
    ("bnt", Abbrev::Tz(28800)),
    ("bort", Abbrev::Tz(28800)),
    ("bot", Abbrev::Tz(-14400)),
    ("bra", Abbrev::Tz(-10800)),
    ("brst", Abbrev::Dtz(-7200)),
    ("brt", Abbrev::Tz(-10800)),
    ("bst", Abbrev::Dtz(3600)),
    ("btt", Abbrev::Tz(21600)),
    ("cadt", Abbrev::Dtz(37800)),
    ("cast", Abbrev::Tz(34200)),
    ("cct", Abbrev::Tz(28800)),
    ("cdt", Abbrev::Dtz(-18000)),
    ("cest", Abbrev::Dtz(7200)),
    ("cet", Abbrev::Tz(3600)),
    ("cetdst", Abbrev::Dtz(7200)),
    ("chadt", Abbrev::Dtz(49500)),
    ("chast", Abbrev::Tz(45900)),
    ("chut", Abbrev::Tz(36000)),
    ("ckt", Abbrev::Dyn),
    ("clst", Abbrev::Dtz(-10800)),
    ("clt", Abbrev::Dyn),
    ("cot", Abbrev::Tz(-18000)),
    ("cst", Abbrev::Tz(-21600)),
    ("cxt", Abbrev::Tz(25200)),
    ("davt", Abbrev::Dyn),
    ("ddut", Abbrev::Tz(36000)),
    ("easst", Abbrev::Dyn),
    ("east", Abbrev::Dyn),
    ("eat", Abbrev::Tz(10800)),
    ("edt", Abbrev::Dtz(-14400)),
    ("eest", Abbrev::Dtz(10800)),
    ("eet", Abbrev::Tz(7200)),
    ("eetdst", Abbrev::Dtz(10800)),
    ("egst", Abbrev::Dtz(0)),
    ("egt", Abbrev::Tz(-3600)),
    ("est", Abbrev::Tz(-18000)),
    ("fet", Abbrev::Tz(10800)),
    ("fjst", Abbrev::Dtz(46800)),
    ("fjt", Abbrev::Tz(43200)),
    ("fkst", Abbrev::Dyn),
    ("fkt", Abbrev::Dyn),
    ("fnst", Abbrev::Dtz(-3600)),
    ("fnt", Abbrev::Tz(-7200)),
    ("galt", Abbrev::Tz(-21600)),
    ("gamt", Abbrev::Tz(-32400)),
    ("gest", Abbrev::Dyn),
    ("get", Abbrev::Dyn),
    ("gft", Abbrev::Tz(-10800)),
    ("gilt", Abbrev::Tz(43200)),
    ("gmt", Abbrev::Tz(0)),
    ("gyt", Abbrev::Dyn),
    ("hkt", Abbrev::Tz(28800)),
    ("hst", Abbrev::Tz(-36000)),
    ("ict", Abbrev::Tz(25200)),
    ("idt", Abbrev::Dtz(10800)),
    ("iot", Abbrev::Dyn),
    ("irkst", Abbrev::Dyn),
    ("irkt", Abbrev::Dyn),
    ("irt", Abbrev::Tz(12600)),
    ("ist", Abbrev::Tz(7200)),
    ("jayt", Abbrev::Tz(32400)),
    ("jst", Abbrev::Tz(32400)),
    ("kdt", Abbrev::Dtz(36000)),
    ("kgst", Abbrev::Dtz(21600)),
    ("kgt", Abbrev::Dyn),
    ("kost", Abbrev::Dyn),
    ("krast", Abbrev::Dyn),
    ("krat", Abbrev::Dyn),
    ("kst", Abbrev::Tz(32400)),
    ("lhdt", Abbrev::Dyn),
    ("lhst", Abbrev::Tz(37800)),
    ("ligt", Abbrev::Tz(36000)),
    ("lint", Abbrev::Dyn),
    ("lkt", Abbrev::Dyn),
    ("magst", Abbrev::Dyn),
    ("magt", Abbrev::Dyn),
    ("mart", Abbrev::Tz(-34200)),
    ("mawt", Abbrev::Dyn),
    ("mdt", Abbrev::Dtz(-21600)),
    ("mest", Abbrev::Dtz(7200)),
    ("mesz", Abbrev::Dtz(7200)),
    ("met", Abbrev::Tz(3600)),
    ("metdst", Abbrev::Dtz(7200)),
    ("mez", Abbrev::Tz(3600)),
    ("mht", Abbrev::Tz(43200)),
    ("mmt", Abbrev::Tz(23400)),
    ("mpt", Abbrev::Tz(36000)),
    ("msd", Abbrev::Dtz(14400)),
    ("msk", Abbrev::Dyn),
    ("mst", Abbrev::Tz(-25200)),
    ("must", Abbrev::Dtz(18000)),
    ("mut", Abbrev::Tz(14400)),
    ("mvt", Abbrev::Tz(18000)),
    ("myt", Abbrev::Tz(28800)),
    ("ndt", Abbrev::Dtz(-9000)),
    ("nft", Abbrev::Tz(-12600)),
    ("novst", Abbrev::Dyn),
    ("novt", Abbrev::Dyn),
    ("npt", Abbrev::Tz(20700)),
    ("nst", Abbrev::Tz(-12600)),
    ("nut", Abbrev::Dyn),
    ("nzdt", Abbrev::Dtz(46800)),
    ("nzst", Abbrev::Tz(43200)),
    ("nzt", Abbrev::Tz(43200)),
    ("omsst", Abbrev::Dyn),
    ("omst", Abbrev::Dyn),
    ("pdt", Abbrev::Dtz(-25200)),
    ("pet", Abbrev::Tz(-18000)),
    ("petst", Abbrev::Dyn),
    ("pett", Abbrev::Dyn),
    ("pgt", Abbrev::Tz(36000)),
    ("pht", Abbrev::Tz(28800)),
    ("pkst", Abbrev::Dtz(21600)),
    ("pkt", Abbrev::Tz(18000)),
    ("pmdt", Abbrev::Dtz(-7200)),
    ("pmst", Abbrev::Tz(-10800)),
    ("pont", Abbrev::Tz(39600)),
    ("pst", Abbrev::Tz(-28800)),
    ("pwt", Abbrev::Tz(32400)),
    ("pyst", Abbrev::Dtz(-10800)),
    ("pyt", Abbrev::Dyn),
    ("ret", Abbrev::Tz(14400)),
    ("sadt", Abbrev::Dtz(37800)),
    ("sast", Abbrev::Tz(7200)),
    ("sct", Abbrev::Tz(14400)),
    ("sgt", Abbrev::Dyn),
    ("taht", Abbrev::Tz(-36000)),
    ("tft", Abbrev::Tz(18000)),
    ("tjt", Abbrev::Tz(18000)),
    ("tkt", Abbrev::Dyn),
    ("tmt", Abbrev::Dyn),
    ("tot", Abbrev::Tz(46800)),
    ("trut", Abbrev::Tz(36000)),
    ("tvt", Abbrev::Tz(43200)),
    ("uct", Abbrev::Tz(0)),
    ("ulast", Abbrev::Dtz(32400)),
    ("ulat", Abbrev::Dyn),
    ("ut", Abbrev::Tz(0)),
    ("utc", Abbrev::Tz(0)),
    ("uyst", Abbrev::Dtz(-7200)),
    ("uyt", Abbrev::Tz(-10800)),
    ("uzst", Abbrev::Dtz(21600)),
    ("uzt", Abbrev::Tz(18000)),
    ("vet", Abbrev::Dyn),
    ("vlast", Abbrev::Dyn),
    ("vlat", Abbrev::Dyn),
    ("volt", Abbrev::Dyn),
    ("vut", Abbrev::Tz(39600)),
    ("wadt", Abbrev::Dtz(28800)),
    ("wakt", Abbrev::Tz(43200)),
    ("wast", Abbrev::Tz(25200)),
    ("wat", Abbrev::Tz(3600)),
    ("wdt", Abbrev::Dtz(32400)),
    ("wet", Abbrev::Tz(0)),
    ("wetdst", Abbrev::Dtz(3600)),
    ("wft", Abbrev::Tz(43200)),
    ("wgst", Abbrev::Dtz(-7200)),
    ("wgt", Abbrev::Tz(-10800)),
    ("xjt", Abbrev::Tz(21600)),
    ("yakst", Abbrev::Dyn),
    ("yakt", Abbrev::Dyn),
    ("yapt", Abbrev::Tz(36000)),
    ("yekst", Abbrev::Dtz(21600)),
    ("yekt", Abbrev::Dyn),
    ("z", Abbrev::Tz(0)),
    ("zulu", Abbrev::Tz(0)),
];

/// Zone names every current tz database install carries (IANA tzdata
/// 2026c, minus the backward-compatibility links), lowercased and sorted.
const KNOWN_ZONES: &[&str] = &[
    "africa/abidjan",
    "africa/accra",
    "africa/addis_ababa",
    "africa/algiers",
    "africa/asmara",
    "africa/bamako",
    "africa/bangui",
    "africa/banjul",
    "africa/bissau",
    "africa/blantyre",
    "africa/brazzaville",
    "africa/bujumbura",
    "africa/cairo",
    "africa/casablanca",
    "africa/ceuta",
    "africa/conakry",
    "africa/dakar",
    "africa/dar_es_salaam",
    "africa/djibouti",
    "africa/douala",
    "africa/el_aaiun",
    "africa/freetown",
    "africa/gaborone",
    "africa/harare",
    "africa/johannesburg",
    "africa/juba",
    "africa/kampala",
    "africa/khartoum",
    "africa/kigali",
    "africa/kinshasa",
    "africa/lagos",
    "africa/libreville",
    "africa/lome",
    "africa/luanda",
    "africa/lubumbashi",
    "africa/lusaka",
    "africa/malabo",
    "africa/maputo",
    "africa/maseru",
    "africa/mbabane",
    "africa/mogadishu",
    "africa/monrovia",
    "africa/nairobi",
    "africa/ndjamena",
    "africa/niamey",
    "africa/nouakchott",
    "africa/ouagadougou",
    "africa/porto-novo",
    "africa/sao_tome",
    "africa/timbuktu",
    "africa/tripoli",
    "africa/tunis",
    "africa/windhoek",
    "america/adak",
    "america/anchorage",
    "america/anguilla",
    "america/antigua",
    "america/araguaina",
    "america/argentina/buenos_aires",
    "america/argentina/catamarca",
    "america/argentina/cordoba",
    "america/argentina/jujuy",
    "america/argentina/la_rioja",
    "america/argentina/mendoza",
    "america/argentina/rio_gallegos",
    "america/argentina/salta",
    "america/argentina/san_juan",
    "america/argentina/san_luis",
    "america/argentina/tucuman",
    "america/argentina/ushuaia",
    "america/aruba",
    "america/asuncion",
    "america/atikokan",
    "america/atka",
    "america/bahia",
    "america/bahia_banderas",
    "america/barbados",
    "america/belem",
    "america/belize",
    "america/blanc-sablon",
    "america/boa_vista",
    "america/bogota",
    "america/boise",
    "america/cambridge_bay",
    "america/campo_grande",
    "america/cancun",
    "america/caracas",
    "america/cayenne",
    "america/cayman",
    "america/chicago",
    "america/chihuahua",
    "america/ciudad_juarez",
    "america/coral_harbour",
    "america/costa_rica",
    "america/coyhaique",
    "america/creston",
    "america/cuiaba",
    "america/curacao",
    "america/danmarkshavn",
    "america/dawson",
    "america/dawson_creek",
    "america/denver",
    "america/detroit",
    "america/dominica",
    "america/edmonton",
    "america/eirunepe",
    "america/el_salvador",
    "america/ensenada",
    "america/fort_nelson",
    "america/fortaleza",
    "america/glace_bay",
    "america/goose_bay",
    "america/grand_turk",
    "america/grenada",
    "america/guadeloupe",
    "america/guatemala",
    "america/guayaquil",
    "america/guyana",
    "america/halifax",
    "america/havana",
    "america/hermosillo",
    "america/indiana/indianapolis",
    "america/indiana/knox",
    "america/indiana/marengo",
    "america/indiana/petersburg",
    "america/indiana/tell_city",
    "america/indiana/vevay",
    "america/indiana/vincennes",
    "america/indiana/winamac",
    "america/inuvik",
    "america/iqaluit",
    "america/jamaica",
    "america/juneau",
    "america/kentucky/louisville",
    "america/kentucky/monticello",
    "america/kralendijk",
    "america/la_paz",
    "america/lima",
    "america/los_angeles",
    "america/lower_princes",
    "america/maceio",
    "america/managua",
    "america/manaus",
    "america/marigot",
    "america/martinique",
    "america/matamoros",
    "america/mazatlan",
    "america/menominee",
    "america/merida",
    "america/metlakatla",
    "america/mexico_city",
    "america/miquelon",
    "america/moncton",
    "america/monterrey",
    "america/montevideo",
    "america/montreal",
    "america/montserrat",
    "america/nassau",
    "america/new_york",
    "america/nipigon",
    "america/nome",
    "america/noronha",
    "america/north_dakota/beulah",
    "america/north_dakota/center",
    "america/north_dakota/new_salem",
    "america/nuuk",
    "america/ojinaga",
    "america/panama",
    "america/pangnirtung",
    "america/paramaribo",
    "america/phoenix",
    "america/port-au-prince",
    "america/port_of_spain",
    "america/porto_acre",
    "america/porto_velho",
    "america/puerto_rico",
    "america/punta_arenas",
    "america/rainy_river",
    "america/rankin_inlet",
    "america/recife",
    "america/regina",
    "america/resolute",
    "america/rio_branco",
    "america/santa_isabel",
    "america/santarem",
    "america/santiago",
    "america/santo_domingo",
    "america/sao_paulo",
    "america/scoresbysund",
    "america/shiprock",
    "america/sitka",
    "america/st_barthelemy",
    "america/st_johns",
    "america/st_kitts",
    "america/st_lucia",
    "america/st_thomas",
    "america/st_vincent",
    "america/swift_current",
    "america/tegucigalpa",
    "america/thule",
    "america/thunder_bay",
    "america/tijuana",
    "america/toronto",
    "america/tortola",
    "america/vancouver",
    "america/virgin",
    "america/whitehorse",
    "america/winnipeg",
    "america/yakutat",
    "america/yellowknife",
    "antarctica/casey",
    "antarctica/davis",
    "antarctica/dumontdurville",
    "antarctica/macquarie",
    "antarctica/mawson",
    "antarctica/mcmurdo",
    "antarctica/palmer",
    "antarctica/rothera",
    "antarctica/syowa",
    "antarctica/troll",
    "antarctica/vostok",
    "arctic/longyearbyen",
    "asia/aden",
    "asia/almaty",
    "asia/amman",
    "asia/anadyr",
    "asia/aqtau",
    "asia/aqtobe",
    "asia/ashgabat",
    "asia/atyrau",
    "asia/baghdad",
    "asia/bahrain",
    "asia/baku",
    "asia/bangkok",
    "asia/barnaul",
    "asia/beirut",
    "asia/bishkek",
    "asia/brunei",
    "asia/chita",
    "asia/chongqing",
    "asia/colombo",
    "asia/damascus",
    "asia/dhaka",
    "asia/dili",
    "asia/dubai",
    "asia/dushanbe",
    "asia/famagusta",
    "asia/gaza",
    "asia/harbin",
    "asia/hebron",
    "asia/ho_chi_minh",
    "asia/hong_kong",
    "asia/hovd",
    "asia/irkutsk",
    "asia/istanbul",
    "asia/jakarta",
    "asia/jayapura",
    "asia/jerusalem",
    "asia/kabul",
    "asia/kamchatka",
    "asia/karachi",
    "asia/kashgar",
    "asia/kathmandu",
    "asia/khandyga",
    "asia/kolkata",
    "asia/krasnoyarsk",
    "asia/kuala_lumpur",
    "asia/kuching",
    "asia/kuwait",
    "asia/macau",
    "asia/magadan",
    "asia/makassar",
    "asia/manila",
    "asia/muscat",
    "asia/nicosia",
    "asia/novokuznetsk",
    "asia/novosibirsk",
    "asia/omsk",
    "asia/oral",
    "asia/phnom_penh",
    "asia/pontianak",
    "asia/pyongyang",
    "asia/qatar",
    "asia/qostanay",
    "asia/qyzylorda",
    "asia/riyadh",
    "asia/sakhalin",
    "asia/samarkand",
    "asia/seoul",
    "asia/shanghai",
    "asia/singapore",
    "asia/srednekolymsk",
    "asia/taipei",
    "asia/tashkent",
    "asia/tbilisi",
    "asia/tehran",
    "asia/tel_aviv",
    "asia/thimphu",
    "asia/tokyo",
    "asia/tomsk",
    "asia/ulaanbaatar",
    "asia/urumqi",
    "asia/ust-nera",
    "asia/vientiane",
    "asia/vladivostok",
    "asia/yakutsk",
    "asia/yangon",
    "asia/yekaterinburg",
    "asia/yerevan",
    "atlantic/azores",
    "atlantic/bermuda",
    "atlantic/canary",
    "atlantic/cape_verde",
    "atlantic/faroe",
    "atlantic/jan_mayen",
    "atlantic/madeira",
    "atlantic/reykjavik",
    "atlantic/south_georgia",
    "atlantic/st_helena",
    "atlantic/stanley",
    "australia/adelaide",
    "australia/brisbane",
    "australia/broken_hill",
    "australia/canberra",
    "australia/currie",
    "australia/darwin",
    "australia/eucla",
    "australia/hobart",
    "australia/lindeman",
    "australia/lord_howe",
    "australia/melbourne",
    "australia/perth",
    "australia/sydney",
    "australia/yancowinna",
    "etc/gmt",
    "etc/gmt+0",
    "etc/gmt+1",
    "etc/gmt+10",
    "etc/gmt+11",
    "etc/gmt+12",
    "etc/gmt+2",
    "etc/gmt+3",
    "etc/gmt+4",
    "etc/gmt+5",
    "etc/gmt+6",
    "etc/gmt+7",
    "etc/gmt+8",
    "etc/gmt+9",
    "etc/gmt-0",
    "etc/gmt-1",
    "etc/gmt-10",
    "etc/gmt-11",
    "etc/gmt-12",
    "etc/gmt-13",
    "etc/gmt-14",
    "etc/gmt-2",
    "etc/gmt-3",
    "etc/gmt-4",
    "etc/gmt-5",
    "etc/gmt-6",
    "etc/gmt-7",
    "etc/gmt-8",
    "etc/gmt-9",
    "etc/gmt0",
    "etc/greenwich",
    "etc/uct",
    "etc/universal",
    "etc/utc",
    "etc/zulu",
    "europe/amsterdam",
    "europe/andorra",
    "europe/astrakhan",
    "europe/athens",
    "europe/belfast",
    "europe/belgrade",
    "europe/berlin",
    "europe/bratislava",
    "europe/brussels",
    "europe/bucharest",
    "europe/budapest",
    "europe/busingen",
    "europe/chisinau",
    "europe/copenhagen",
    "europe/dublin",
    "europe/gibraltar",
    "europe/guernsey",
    "europe/helsinki",
    "europe/isle_of_man",
    "europe/istanbul",
    "europe/jersey",
    "europe/kaliningrad",
    "europe/kirov",
    "europe/kyiv",
    "europe/lisbon",
    "europe/ljubljana",
    "europe/london",
    "europe/luxembourg",
    "europe/madrid",
    "europe/malta",
    "europe/mariehamn",
    "europe/minsk",
    "europe/monaco",
    "europe/moscow",
    "europe/nicosia",
    "europe/oslo",
    "europe/paris",
    "europe/podgorica",
    "europe/prague",
    "europe/riga",
    "europe/rome",
    "europe/samara",
    "europe/san_marino",
    "europe/sarajevo",
    "europe/saratov",
    "europe/simferopol",
    "europe/skopje",
    "europe/sofia",
    "europe/stockholm",
    "europe/tallinn",
    "europe/tirane",
    "europe/tiraspol",
    "europe/ulyanovsk",
    "europe/vaduz",
    "europe/vatican",
    "europe/vienna",
    "europe/vilnius",
    "europe/volgograd",
    "europe/warsaw",
    "europe/zagreb",
    "europe/zurich",
    "factory",
    "gmt",
    "indian/antananarivo",
    "indian/chagos",
    "indian/christmas",
    "indian/cocos",
    "indian/comoro",
    "indian/kerguelen",
    "indian/mahe",
    "indian/maldives",
    "indian/mauritius",
    "indian/mayotte",
    "indian/reunion",
    "pacific/apia",
    "pacific/auckland",
    "pacific/bougainville",
    "pacific/chatham",
    "pacific/chuuk",
    "pacific/easter",
    "pacific/efate",
    "pacific/fakaofo",
    "pacific/fiji",
    "pacific/funafuti",
    "pacific/galapagos",
    "pacific/gambier",
    "pacific/guadalcanal",
    "pacific/guam",
    "pacific/honolulu",
    "pacific/johnston",
    "pacific/kanton",
    "pacific/kiritimati",
    "pacific/kosrae",
    "pacific/kwajalein",
    "pacific/majuro",
    "pacific/marquesas",
    "pacific/midway",
    "pacific/nauru",
    "pacific/niue",
    "pacific/norfolk",
    "pacific/noumea",
    "pacific/pago_pago",
    "pacific/palau",
    "pacific/pitcairn",
    "pacific/pohnpei",
    "pacific/port_moresby",
    "pacific/rarotonga",
    "pacific/saipan",
    "pacific/samoa",
    "pacific/tahiti",
    "pacific/tarawa",
    "pacific/tongatapu",
    "pacific/wake",
    "pacific/wallis",
    "pacific/yap",
    "utc",
];

/// IANA backward-compatibility links: present only where the distribution
/// installs them (Debian moved them to `tzdata-legacy`).
const LEGACY_ZONES: &[&str] = &[
    "africa/asmera",
    "america/argentina/comodrivadavia",
    "america/buenos_aires",
    "america/catamarca",
    "america/cordoba",
    "america/fort_wayne",
    "america/godthab",
    "america/indianapolis",
    "america/jujuy",
    "america/knox_in",
    "america/louisville",
    "america/mendoza",
    "america/rosario",
    "antarctica/south_pole",
    "asia/ashkhabad",
    "asia/calcutta",
    "asia/choibalsan",
    "asia/chungking",
    "asia/dacca",
    "asia/katmandu",
    "asia/macao",
    "asia/rangoon",
    "asia/saigon",
    "asia/thimbu",
    "asia/ujung_pandang",
    "asia/ulan_bator",
    "atlantic/faeroe",
    "australia/act",
    "australia/lhi",
    "australia/north",
    "australia/nsw",
    "australia/queensland",
    "australia/south",
    "australia/tasmania",
    "australia/victoria",
    "australia/west",
    "brazil/acre",
    "brazil/denoronha",
    "brazil/east",
    "brazil/west",
    "canada/atlantic",
    "canada/central",
    "canada/eastern",
    "canada/mountain",
    "canada/newfoundland",
    "canada/pacific",
    "canada/saskatchewan",
    "canada/yukon",
    "cet",
    "chile/continental",
    "chile/easterisland",
    "cst6cdt",
    "cuba",
    "eet",
    "egypt",
    "eire",
    "est",
    "est5edt",
    "europe/kiev",
    "europe/uzhgorod",
    "europe/zaporozhye",
    "gb",
    "gb-eire",
    "gmt+0",
    "gmt-0",
    "gmt0",
    "greenwich",
    "hongkong",
    "hst",
    "iceland",
    "iran",
    "israel",
    "jamaica",
    "japan",
    "kwajalein",
    "libya",
    "met",
    "mexico/bajanorte",
    "mexico/bajasur",
    "mexico/general",
    "mst",
    "mst7mdt",
    "navajo",
    "nz",
    "nz-chat",
    "pacific/enderbury",
    "pacific/ponape",
    "pacific/truk",
    "poland",
    "portugal",
    "prc",
    "pst8pdt",
    "roc",
    "rok",
    "singapore",
    "turkey",
    "uct",
    "universal",
    "us/alaska",
    "us/aleutian",
    "us/arizona",
    "us/central",
    "us/east-indiana",
    "us/eastern",
    "us/hawaii",
    "us/indiana-starke",
    "us/michigan",
    "us/mountain",
    "us/pacific",
    "us/samoa",
    "w-su",
    "wet",
    "zulu",
];
