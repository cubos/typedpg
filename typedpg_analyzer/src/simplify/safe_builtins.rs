//! The built-in casts, functions and operators [`super::proof::cannot_error`]
//! accepts: each one returns a value for every input, never an error. The
//! lists are deliberately short; an entry belongs here only once its C
//! implementation (PostgreSQL 18.6, cited per entry) has been read and it
//! has been run on the extreme values of its input type (min / max,
//! NaN and ±infinity, empty and huge values). Errors that are about the
//! server rather than the value — running out of memory, a broken ICU
//! library — are outside every proof here, as they are for the text
//! comparisons the proof already accepted.
//!
//! Everything is identified by OID, which the catalog files fix for every
//! installation: a user object of the same name has another OID and is
//! refused.

use crate::oid::{PgOperatorOid, PgProcOid, PgTypeOid};
use crate::pg_catalog::oid;

/// Cast functions (`pg_cast.castfunc`) that can't fail, implicit or
/// explicit:
///
/// - integer widening — `i2toi4` (313), `int28` (754), `int48` (481)
///   (`int.c`, `int8.c`): a C widening, `PG_RETURN_INT64((int64) arg)`;
/// - integer to `numeric` — `int2_numeric` (1782), `int4_numeric` (1740),
///   `int8_numeric` (1781) (`numeric.c`): `int64_to_numeric`, whose only
///   error is a weight past `NUMERIC_WEIGHT_MAX`, far beyond 19 digits;
/// - integer to `float4` / `float8` — `i2tof` (236), `i4tof` (318),
///   `i8tof` (652), `i2tod` (235), `i4tod` (316), `i8tod` (482)
///   (`float.c`, `int8.c`): a C conversion with no range check. `int8` and
///   `int4` to `float4` (and `int8` to `float8`) round, but every integer
///   is far inside the float range: `9223372036854775807::float4` is
///   `9.223372e+18`;
/// - `float4` to `float8` — `ftod` (311) (`float.c`): exact, NaN and
///   ±infinity included;
/// - `bool` to `text` / `varchar` / `bpchar` — `booltext` (2971)
///   (`bool.c`): `cstring_to_text("true" / "false")`;
/// - `name` to `text` / `varchar` — `name_text` (406, 1401) (`varlena.c`):
///   `cstring_to_text(NameStr(*s))`;
/// - `bpchar` to `text` / `varchar` — `rtrim1` (401) (`oracle_compat.c`):
///   strips trailing spaces, `dotrim` raises nothing.
///
/// Not here, as they fail on some value (PG 18.6):
///
/// - every narrowing — `int84`, `int82`, `i4toi2` (`integer out of
///   range`), `numeric_int*`, `dtoi*`, `ftoi*`;
/// - `numeric` to `float8` / `float4` (`numeric_float8` goes through
///   `float8in`: `'1e400'::numeric::float8` is out of range for type
///   double precision), `float8` to `float4` (`dtof`: `value out of range:
///   overflow`);
/// - `date` to `timestamp` / `timestamptz` (`date2timestamp`: `date out of
///   range for timestamp` past 294276 AD, `'5874897-12-31'::date`);
/// - `timestamp` to `timestamptz` and back (`timestamp out of range`
///   where the time zone offset crosses a range end, `'294276-12-31
///   23:00'::timestamp::timestamptz` under `America/Los_Angeles`);
/// - a length coercion (`varchar(n)`, `numeric(p, s)`, `timestamp(p)`):
///   they are refused with every typmod.
///
/// `date` and `timestamp` compared with `timestamptz` need none of those
/// casts: PG has cross-type comparison operators for them, which treat an
/// overflowing conversion as ±infinity instead of failing
/// (`date2timestamptz_opt_overflow`).
pub(crate) const CAST_FUNCTIONS: &[u32] = &[
    313, 754, 481, // int2 -> int4, int2 -> int8, int4 -> int8
    1782, 1740, 1781, // int2 / int4 / int8 -> numeric
    236, 318, 652, // int2 / int4 / int8 -> float4
    235, 316, 482,  // int2 / int4 / int8 -> float8
    311,  // float4 -> float8
    2971, // bool -> text / varchar / bpchar
    406, 1401, // name -> text, name -> varchar
    401,  // bpchar -> text / varchar
];

/// Source types whose I/O conversion to `text` (`x::text`, which has no
/// `pg_cast` row: an output function, then `textin`) can't fail:
/// `int2out`, `int4out`, `int8out` (`pg_itoa` / `pg_ltoa` / `pg_lltoa`
/// into a fixed buffer) and `numeric_out` (raises nothing, NaN and
/// ±Infinity included); `textin` is `cstring_to_text`.
pub(crate) const IO_TO_TEXT: &[PgTypeOid] = &[oid::INT2, oid::INT4, oid::INT8, oid::NUMERIC];

/// Functions that can't fail, whatever their argument (each strict, so a
/// NULL argument never reaches it):
///
/// - `lower(text)` (870), `upper(text)` (871) (`oracle_compat.c` →
///   `str_tolower` / `str_toupper` in `formatting.c`): their one
///   value-independent error is `could not determine which collation to
///   use`, which needs a collation conflict the proof refuses; the
///   provider paths (`pg_strlower`) raise only out-of-memory and ICU
///   failures;
/// - `length` / `char_length` / `character_length` (1317, 1381, 1369) and
///   `octet_length` (1374) of `text`, the same of `bpchar` (1318, 1372,
///   1367, 1375) (`varlena.c` `textlen`, `textoctetlen`; `varchar.c`
///   `bpcharlen`, `bpcharoctetlen`): a byte count, or `pg_mbstrlen_with_len`,
///   which doesn't validate;
/// - `abs(float4)` (1394), `abs(float8)` (1395) (`float.c`: `fabsf` /
///   `fabs`) and `abs(numeric)` (1705) (`numeric.c`: clears the sign bit;
///   NaN stays NaN, -Infinity becomes Infinity). Not the integer ones:
///   `abs(-2147483648)` is `integer out of range`;
/// - `now()` (1299), `transaction_timestamp()` (2647),
///   `statement_timestamp()` (2648) (`timestamp.c`): a stored timestamp,
///   returned as is. They are STABLE, not volatile: the same value
///   wherever and however often the statement evaluates them.
pub(crate) const FUNCTIONS: &[u32] = &[
    870, 871, // lower(text), upper(text)
    1317, 1381, 1369, 1374, // length, char_length, character_length, octet_length (text)
    1318, 1372, 1367, 1375, // the same of bpchar
    1394, 1395, 1705, // abs(float4), abs(float8), abs(numeric)
    1299, 2647, 2648, // now(), transaction_timestamp(), statement_timestamp()
];

/// Whether `f` is one of [`FUNCTIONS`].
pub(crate) fn function(f: PgProcOid) -> bool {
    FUNCTIONS.contains(&f.get())
}

/// Whether `f` is one of [`CAST_FUNCTIONS`].
pub(crate) fn cast_function(f: PgProcOid) -> bool {
    CAST_FUNCTIONS.contains(&f.get())
}

/// The functions that read the transaction's or statement's start time:
/// `now()`, `transaction_timestamp()`, `statement_timestamp()`.
pub(crate) const CLOCK_FUNCTIONS: &[u32] = &[1299, 2647, 2648];

/// The operators that shift a date or timestamp by an interval or a
/// number of days: `date ± interval` (1076, 1077: `date_pl_interval`,
/// `date_mi_interval`), `date ± integer` (1100, 1101: `date_pli`,
/// `date_mii`), `timestamptz ± interval` (1327, 1329:
/// `timestamptz_pl_interval`, `timestamptz_mi_interval`) and `timestamp ±
/// interval` (2066, 2068). Each fails only when the result leaves the
/// type's range (`timestamp out of range`, `date out of range`), so
/// `cannot_error` accepts them only on a clock reading shifted by a
/// bounded constant ([`MAX_SHIFT_YEARS`]).
pub(crate) const CLOCK_SHIFT_OPERATORS: &[u32] = &[1076, 1077, 1100, 1101, 1327, 1329, 2066, 2068];

/// Whether `op` is one of [`CLOCK_SHIFT_OPERATORS`].
pub(crate) fn clock_shift_operator(op: PgOperatorOid) -> bool {
    CLOCK_SHIFT_OPERATORS.contains(&op.get())
}

/// The largest shift, per interval field (months, days, time), that
/// `cannot_error` accepts on a clock reading. A clock reading is the
/// server's time — between 1970 and the present give or take a clock
/// error, nowhere near the ends of the `timestamp` range (4714 BC to
/// 294276 AD) or the `date` range (4714 BC to 5874897 AD) — and three
/// fields of at most 1000 years each move it by at most 3000 years.
pub(crate) const MAX_SHIFT_YEARS: i64 = 1000;
