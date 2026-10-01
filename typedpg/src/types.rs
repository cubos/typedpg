//! Rust types for the PostgreSQL types no common crate represents.
//!
//! The `sql!` macro reads and binds these built-in types as:
//!
//! | PostgreSQL | Rust |
//! |------------|------|
//! | `interval` | [`Interval`] |
//! | `timetz` | [`TimeTz`] |
//! | `pg_lsn` | [`PgLsn`] |
//! | `xid8` | [`Xid8`] |
//! | `int4range`, `int8range`, `numrange`, `daterange`, `tsrange`, `tstzrange` (and any `CREATE TYPE ... AS RANGE`) | [`Range<T>`] |
//! | `int4multirange`, … (the built-in multiranges) | [`MultiRange<T>`] |
//!
//! Each implements `tokio_postgres`'s `FromSql` / `ToSql` for its type's
//! binary format, so it also works with the raw client.

use std::error::Error as StdError;
use std::ops::Bound;

use bytes::{Buf, BufMut, BytesMut};
use tokio_postgres::types::{FromSql, IsNull, Kind, ToSql, Type, to_sql_checked};

pub use tokio_postgres::types::PgLsn;

type BoxError = Box<dyn StdError + Sync + Send>;

/// Fails unless `raw` is exactly `len` bytes long.
fn expect_len(raw: &[u8], len: usize, ty: &str) -> Result<(), BoxError> {
    if raw.len() != len {
        return Err(format!("invalid {ty} value: {} bytes, expected {len}", raw.len()).into());
    }
    Ok(())
}

/// A PostgreSQL `interval`: months, days and microseconds, kept apart as
/// PostgreSQL keeps them (a month has no fixed number of days, nor a day of
/// seconds, across a daylight-saving change).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Interval {
    pub months: i32,
    pub days: i32,
    pub microseconds: i64,
}

impl Interval {
    /// An interval of `months`, `days` and `microseconds`.
    pub const fn new(months: i32, days: i32, microseconds: i64) -> Self {
        Interval {
            months,
            days,
            microseconds,
        }
    }
}

impl<'a> FromSql<'a> for Interval {
    fn from_sql(_: &Type, mut raw: &'a [u8]) -> Result<Self, BoxError> {
        expect_len(raw, 16, "interval")?;
        let microseconds = raw.get_i64();
        let days = raw.get_i32();
        let months = raw.get_i32();
        Ok(Interval {
            months,
            days,
            microseconds,
        })
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::INTERVAL
    }
}

impl ToSql for Interval {
    fn to_sql(&self, _: &Type, out: &mut BytesMut) -> Result<IsNull, BoxError> {
        out.put_i64(self.microseconds);
        out.put_i32(self.days);
        out.put_i32(self.months);
        Ok(IsNull::No)
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::INTERVAL
    }

    to_sql_checked!();
}

/// A PostgreSQL `timetz`: a time of day with a UTC offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimeTz {
    pub time: chrono::NaiveTime,
    pub offset: chrono::FixedOffset,
}

impl<'a> FromSql<'a> for TimeTz {
    fn from_sql(_: &Type, mut raw: &'a [u8]) -> Result<Self, BoxError> {
        expect_len(raw, 12, "timetz")?;
        let micros = raw.get_i64();
        // PostgreSQL stores the zone as seconds *west* of UTC.
        let west = raw.get_i32();
        // 24:00:00 is a valid timetz; chrono has no such time.
        if !(0..86_400_000_000).contains(&micros) {
            return Err(format!("timetz value out of chrono's range: {micros} µs").into());
        }
        let time = chrono::NaiveTime::MIN
            .overflowing_add_signed(chrono::TimeDelta::microseconds(micros))
            .0;
        let offset = chrono::FixedOffset::east_opt(-west)
            .ok_or_else(|| format!("invalid timetz offset: {west} s west of UTC"))?;
        Ok(TimeTz { time, offset })
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::TIMETZ
    }
}

impl ToSql for TimeTz {
    fn to_sql(&self, _: &Type, out: &mut BytesMut) -> Result<IsNull, BoxError> {
        let since_midnight = self.time - chrono::NaiveTime::MIN;
        let micros = since_midnight
            .num_microseconds()
            .ok_or("timetz value out of range")?;
        out.put_i64(micros);
        out.put_i32(-self.offset.local_minus_utc());
        Ok(IsNull::No)
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::TIMETZ
    }

    to_sql_checked!();
}

/// A PostgreSQL `xid8`: a 64-bit, epoch-extended transaction ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Xid8(pub u64);

impl From<u64> for Xid8 {
    fn from(v: u64) -> Self {
        Xid8(v)
    }
}

impl From<Xid8> for u64 {
    fn from(v: Xid8) -> Self {
        v.0
    }
}

impl<'a> FromSql<'a> for Xid8 {
    fn from_sql(_: &Type, mut raw: &'a [u8]) -> Result<Self, BoxError> {
        expect_len(raw, 8, "xid8")?;
        Ok(Xid8(raw.get_u64()))
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::XID8
    }
}

impl ToSql for Xid8 {
    fn to_sql(&self, _: &Type, out: &mut BytesMut) -> Result<IsNull, BoxError> {
        out.put_u64(self.0);
        Ok(IsNull::No)
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::XID8
    }

    to_sql_checked!();
}

/// A value of a PostgreSQL range type over `T`.
///
/// PostgreSQL canonicalizes a discrete range (`int4range`, `int8range`,
/// `daterange`) to an inclusive lower and exclusive upper bound, so
/// `'[1,5]'::int4range` reads back as `1..6`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Range<T> {
    /// The empty range.
    Empty,
    /// A range with these bounds.
    NonEmpty { lower: Bound<T>, upper: Bound<T> },
}

impl<T> Range<T> {
    /// The range between `lower` and `upper`.
    pub fn new(lower: Bound<T>, upper: Bound<T>) -> Self {
        Range::NonEmpty { lower, upper }
    }
}

// Range flags, from PostgreSQL's rangetypes.h.
const RANGE_EMPTY: u8 = 0x01;
const RANGE_LB_INC: u8 = 0x02;
const RANGE_UB_INC: u8 = 0x04;
const RANGE_LB_INF: u8 = 0x08;
const RANGE_UB_INF: u8 = 0x10;

fn range_subtype(ty: &Type) -> Option<&Type> {
    match ty.kind() {
        Kind::Range(subtype) => Some(subtype),
        Kind::Domain(base) => range_subtype(base),
        _ => None,
    }
}

impl<'a, T: FromSql<'a>> FromSql<'a> for Range<T> {
    fn from_sql(ty: &Type, mut raw: &'a [u8]) -> Result<Self, BoxError> {
        let subtype = range_subtype(ty).ok_or("not a range type")?;
        if raw.is_empty() {
            return Err("invalid range value: no flags".into());
        }
        let flags = raw.get_u8();
        if flags & RANGE_EMPTY != 0 {
            return Ok(Range::Empty);
        }
        let mut bound = |infinite: u8, inclusive: u8| -> Result<Bound<T>, BoxError> {
            if flags & infinite != 0 {
                return Ok(Bound::Unbounded);
            }
            if raw.len() < 4 {
                return Err("invalid range value: truncated bound".into());
            }
            let len = raw.get_i32();
            let len = usize::try_from(len).map_err(|_| "invalid range value: NULL bound")?;
            if raw.len() < len {
                return Err("invalid range value: truncated bound".into());
            }
            let (value, rest) = raw.split_at(len);
            raw = rest;
            let value = T::from_sql(subtype, value)?;
            Ok(if flags & inclusive != 0 {
                Bound::Included(value)
            } else {
                Bound::Excluded(value)
            })
        };
        let lower = bound(RANGE_LB_INF, RANGE_LB_INC)?;
        let upper = bound(RANGE_UB_INF, RANGE_UB_INC)?;
        if !raw.is_empty() {
            return Err("invalid range value: trailing bytes".into());
        }
        Ok(Range::NonEmpty { lower, upper })
    }

    fn accepts(ty: &Type) -> bool {
        range_subtype(ty).is_some_and(T::accepts)
    }
}

impl<T: ToSql> ToSql for Range<T> {
    fn to_sql(&self, ty: &Type, out: &mut BytesMut) -> Result<IsNull, BoxError> {
        let subtype = range_subtype(ty).ok_or("not a range type")?;
        let (lower, upper) = match self {
            Range::Empty => {
                out.put_u8(RANGE_EMPTY);
                return Ok(IsNull::No);
            }
            Range::NonEmpty { lower, upper } => (lower, upper),
        };
        let flags_at = out.len();
        out.put_u8(0);
        let mut flags = 0;
        for (bound, infinite, inclusive) in [
            (lower, RANGE_LB_INF, RANGE_LB_INC),
            (upper, RANGE_UB_INF, RANGE_UB_INC),
        ] {
            let value = match bound {
                Bound::Unbounded => {
                    flags |= infinite;
                    continue;
                }
                Bound::Included(v) => {
                    flags |= inclusive;
                    v
                }
                Bound::Excluded(v) => v,
            };
            let len_at = out.len();
            out.put_i32(0);
            if let IsNull::Yes = value.to_sql_checked(subtype, out)? {
                return Err("a range bound can't be NULL".into());
            }
            let len = i32::try_from(out.len() - len_at - 4).map_err(|_| "range bound too large")?;
            out[len_at..len_at + 4].copy_from_slice(&len.to_be_bytes());
        }
        out[flags_at] = flags;
        Ok(IsNull::No)
    }

    fn accepts(ty: &Type) -> bool {
        range_subtype(ty).is_some_and(T::accepts)
    }

    to_sql_checked!();
}

/// A value of a PostgreSQL multirange type over `T` (`int4multirange`, …):
/// its ranges, in PostgreSQL's canonical order (sorted, non-empty, neither
/// overlapping nor adjacent).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct MultiRange<T>(pub Vec<Range<T>>);

/// A range type over the subtype of multirange type `ty`, to read and write
/// its ranges as. Only the built-in multiranges have their subtype in their
/// `Kind`: tokio-postgres describes a user-defined one as a simple type.
fn multirange_range(ty: &Type) -> Option<Type> {
    match ty.kind() {
        Kind::Multirange(subtype) => Some(Type::new(
            format!("{}_range", subtype.name()),
            0,
            Kind::Range(subtype.clone()),
            "pg_catalog".into(),
        )),
        Kind::Domain(base) => multirange_range(base),
        _ => None,
    }
}

impl<'a, T: FromSql<'a>> FromSql<'a> for MultiRange<T> {
    fn from_sql(ty: &Type, mut raw: &'a [u8]) -> Result<Self, BoxError> {
        let range_ty = multirange_range(ty).ok_or("not a built-in multirange type")?;
        if raw.len() < 4 {
            return Err("invalid multirange value: truncated count".into());
        }
        let count = raw.get_u32() as usize;
        let mut ranges = Vec::with_capacity(count.min(raw.len() / 5));
        for _ in 0..count {
            if raw.len() < 4 {
                return Err("invalid multirange value: truncated range".into());
            }
            let len = raw.get_u32() as usize;
            if raw.len() < len {
                return Err("invalid multirange value: truncated range".into());
            }
            let (range, rest) = raw.split_at(len);
            raw = rest;
            ranges.push(Range::<T>::from_sql(&range_ty, range)?);
        }
        if !raw.is_empty() {
            return Err("invalid multirange value: trailing bytes".into());
        }
        Ok(MultiRange(ranges))
    }

    fn accepts(ty: &Type) -> bool {
        multirange_range(ty).is_some_and(|r| <Range<T> as FromSql>::accepts(&r))
    }
}

impl<T: ToSql> ToSql for MultiRange<T> {
    fn to_sql(&self, ty: &Type, out: &mut BytesMut) -> Result<IsNull, BoxError> {
        let range_ty = multirange_range(ty).ok_or("not a built-in multirange type")?;
        let count = u32::try_from(self.0.len()).map_err(|_| "too many ranges")?;
        out.put_u32(count);
        for range in &self.0 {
            let len_at = out.len();
            out.put_u32(0);
            range.to_sql(&range_ty, out)?;
            let len = u32::try_from(out.len() - len_at - 4).map_err(|_| "range too large")?;
            out[len_at..len_at + 4].copy_from_slice(&len.to_be_bytes());
        }
        Ok(IsNull::No)
    }

    fn accepts(ty: &Type) -> bool {
        multirange_range(ty).is_some_and(|r| <Range<T> as ToSql>::accepts(&r))
    }

    to_sql_checked!();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T>(ty: &Type, v: &T) -> T
    where
        T: ToSql + for<'a> FromSql<'a>,
    {
        let mut buf = BytesMut::new();
        v.to_sql_checked(ty, &mut buf).unwrap();
        T::from_sql(ty, &buf).unwrap()
    }

    #[test]
    fn interval_roundtrips() {
        let v = Interval::new(14, -3, 5_000_001);
        assert_eq!(roundtrip(&Type::INTERVAL, &v), v);
    }

    #[test]
    fn timetz_roundtrips() {
        let v = TimeTz {
            time: chrono::NaiveTime::from_hms_micro_opt(13, 4, 5, 6).unwrap(),
            offset: chrono::FixedOffset::east_opt(-3 * 3600).unwrap(),
        };
        assert_eq!(roundtrip(&Type::TIMETZ, &v), v);
    }

    #[test]
    fn ranges_roundtrip() {
        for v in [
            Range::Empty,
            Range::new(Bound::Included(1), Bound::Excluded(6)),
            Range::new(Bound::Unbounded, Bound::Included(3)),
            Range::new(Bound::Excluded(-2), Bound::Unbounded),
        ] {
            assert_eq!(roundtrip::<Range<i32>>(&Type::INT4_RANGE, &v), v);
        }
    }

    #[test]
    fn xid8_roundtrips() {
        assert_eq!(roundtrip(&Type::XID8, &Xid8(u64::MAX)), Xid8(u64::MAX));
    }
}
