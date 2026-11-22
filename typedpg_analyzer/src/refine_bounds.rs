//! Bounds of ordered values that are not integers: `numeric`, `date`,
//! `timestamp` and `timestamptz` (see [`crate::refine::Refinement::bounds`]).
//!
//! Each type's order is total, its special values included: a `numeric`
//! `-Infinity` sorts below every number, `Infinity` above, and `NaN` above
//! everything (`numeric_cmp`); a date's or timestamp's `-infinity` /
//! `infinity` below / above every finite one. A bound is in that order,
//! so `n > 5` leaves `Infinity` and `NaN` in. The floats have no exact
//! order a constant can be compared in, and are never bounded.

use std::cmp::Ordering;
use std::fmt;

use crate::oid::PgTypeOid;
use crate::pg_catalog::PgCatalog;

/// The type whose order a bound is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OrdKind {
    /// `numeric`: a finite point is the number.
    Numeric,
    /// `date`: a finite point is the day, counted from 2000-01-01 (PG's
    /// epoch, as `date_send` sends it).
    Date,
    /// `timestamp`: a finite point is the microsecond, counted from
    /// 2000-01-01 00:00 (as `timestamp_send` sends it).
    Timestamp,
    /// `timestamptz`: a finite point is the microsecond, counted from
    /// 2000-01-01 00:00 UTC (as `timestamptz_send` sends it).
    TimestampTz,
}

impl OrdKind {
    /// The order values of type `t` (or of a domain over it) are in.
    pub(crate) fn of(t: PgTypeOid, snapshot: &PgCatalog) -> Option<OrdKind> {
        use crate::pg_catalog::oid;
        match snapshot.unwrap_domain(t) {
            oid::NUMERIC => Some(OrdKind::Numeric),
            oid::DATE => Some(OrdKind::Date),
            oid::TIMESTAMP => Some(OrdKind::Timestamp),
            oid::TIMESTAMPTZ => Some(OrdKind::TimestampTz),
            _ => None,
        }
    }

    /// What the type's input function makes of `text` (an untyped
    /// literal), when that doesn't depend on the session: `numeric_in` on
    /// a plain decimal or a special value; `date_in` / `timestamp_in` /
    /// `timestamptz_in` on a year-first date (the same under every
    /// `DateStyle`), `epoch` or `±infinity` — never `now`, `today`, … —
    /// and, for a `timestamptz`, only with an explicit zone (the session's
    /// TimeZone is not known). `None` otherwise, or when the input
    /// function rejects it.
    pub(crate) fn read(self, text: &str) -> Option<OrdPoint> {
        let with_time = match self {
            OrdKind::Numeric => return read_numeric(text),
            OrdKind::Date => None,
            OrdKind::Timestamp => Some(false),
            OrdKind::TimestampTz => Some(true),
        };
        Some(
            match crate::datetime_input::datetime_value(text, with_time, false)? {
                i128::MAX => OrdPoint::Infinity,
                i128::MIN => OrdPoint::NegInfinity,
                v => OrdPoint::Finite(Num::int(v)),
            },
        )
    }

    /// Whether the type has infinities a [`crate::refine::Refinement`]'s
    /// `finite` excludes.
    fn datetime(self) -> bool {
        self != OrdKind::Numeric
    }
}

/// `numeric_in`: surrounding blanks, then a special value (`NaN`,
/// `Infinity`, `inf`, signed but for `NaN`, any case) or a decimal
/// (`1.50`, `.5`, `-2e3`). Not the forms PG 16 added (`0x1F`, `1_000`):
/// they are left unread.
fn read_numeric(text: &str) -> Option<OrdPoint> {
    let blank = |c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c');
    let s = text.trim_matches(blank);
    let lower = s.to_ascii_lowercase();
    match lower.as_str() {
        "nan" => return Some(OrdPoint::NaN),
        "infinity" | "+infinity" | "inf" | "+inf" => return Some(OrdPoint::Infinity),
        "-infinity" | "-inf" => return Some(OrdPoint::NegInfinity),
        _ => {}
    }
    Num::parse(s).map(OrdPoint::Finite)
}

/// The most fractional digits a [`Num`] keeps (so comparing two never
/// overflows).
const MAX_SCALE: u32 = 38;

/// An exact decimal, `mantissa × 10^-scale`, normalized: no trailing zero
/// fractional digit, so equal numbers (`1`, `1.0`) are equal values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Num {
    mantissa: i128,
    scale: u32,
}

impl Num {
    /// An integer.
    pub(crate) fn int(i: i128) -> Num {
        Num {
            mantissa: i,
            scale: 0,
        }
    }

    /// The digits, as an integer.
    pub fn mantissa(&self) -> i128 {
        self.mantissa
    }

    /// How many of the digits are fractional.
    pub fn scale(&self) -> u32 {
        self.scale
    }

    /// The value, when it is an integer.
    pub fn to_integer(&self) -> Option<i128> {
        (self.scale == 0).then_some(self.mantissa)
    }

    /// A decimal as written (`1.5`, `.5`, `5.`, `-2.50`, `1e3`): `None`
    /// for anything else, or more digits than an `i128` holds.
    pub(crate) fn parse(s: &str) -> Option<Num> {
        let (neg, s) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s.strip_prefix('+').unwrap_or(s)),
        };
        let (digits, exp) = match s.find(['e', 'E']) {
            Some(i) => {
                let e = &s[i + 1..];
                let unsigned = e.strip_prefix(['+', '-']).unwrap_or(e);
                if unsigned.is_empty() || !unsigned.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                (&s[..i], e.parse::<i64>().ok()?)
            }
            None => (s, 0),
        };
        let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
        if int.is_empty() && frac.is_empty() {
            return None;
        }
        let mut m: i128 = 0;
        for c in int.chars().chain(frac.chars()) {
            let d = c.to_digit(10)?;
            m = m.checked_mul(10)?.checked_add(i128::from(d))?;
        }
        let mut scale = i64::try_from(frac.len()).ok()?.checked_sub(exp)?;
        if scale < 0 {
            m = m.checked_mul(10i128.checked_pow(u32::try_from(-scale).ok()?)?)?;
            scale = 0;
        }
        Num::normalized(if neg { -m } else { m }, u32::try_from(scale).ok()?)
    }

    fn normalized(mut mantissa: i128, mut scale: u32) -> Option<Num> {
        while scale > 0 && mantissa % 10 == 0 {
            mantissa /= 10;
            scale -= 1;
        }
        (scale <= MAX_SCALE).then_some(Num { mantissa, scale })
    }

    /// The integer part (toward zero) and the fractional part, at scale
    /// [`MAX_SCALE`] (under `10^38`, which an `i128` holds).
    fn parts(self) -> (i128, i128) {
        let unit = 10i128.pow(self.scale);
        (
            self.mantissa / unit,
            (self.mantissa % unit) * 10i128.pow(MAX_SCALE - self.scale),
        )
    }
}

impl Ord for Num {
    fn cmp(&self, other: &Num) -> Ordering {
        self.parts().cmp(&other.parts())
    }
}

impl PartialOrd for Num {
    fn partial_cmp(&self, other: &Num) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Num {
    /// The shortest decimal: `1.5`, `-0.25`, `2`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let digits = self.mantissa.unsigned_abs().to_string();
        let sign = if self.mantissa < 0 { "-" } else { "" };
        let scale = self.scale as usize;
        if scale == 0 {
            return write!(f, "{sign}{digits}");
        }
        let padded = format!("{digits:0>width$}", width = scale + 1);
        let (int, frac) = padded.split_at(padded.len() - scale);
        write!(f, "{sign}{int}.{frac}")
    }
}

/// A value in its type's order (see [`OrdKind`] for what a finite one
/// counts). The variants are in that order: `NaN` (only a `numeric`'s)
/// is above everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OrdPoint {
    NegInfinity,
    Finite(Num),
    Infinity,
    NaN,
}

/// One end of an [`OrdRange`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OrdBound {
    pub point: OrdPoint,
    /// Whether the point itself is in the range.
    pub inclusive: bool,
}

/// The bounds every non-NULL value of a `numeric`, `date`, `timestamp` or
/// `timestamptz` lies within, in the type's order (`None`: unbounded).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OrdRange {
    pub kind: OrdKind,
    pub lo: Option<OrdBound>,
    pub hi: Option<OrdBound>,
}

impl OrdRange {
    /// Exactly `p`.
    pub(crate) fn exactly(kind: OrdKind, p: OrdPoint) -> OrdRange {
        let b = Some(OrdBound {
            point: p,
            inclusive: true,
        });
        OrdRange { kind, lo: b, hi: b }
    }

    /// Bounded by nothing.
    pub(crate) fn unbounded(kind: OrdKind) -> OrdRange {
        OrdRange {
            kind,
            lo: None,
            hi: None,
        }
    }

    pub(crate) fn is_unbounded(&self) -> bool {
        self.lo.is_none() && self.hi.is_none()
    }

    /// Whether `p` may be in it.
    pub fn contains(&self, p: OrdPoint) -> bool {
        self.lo
            .is_none_or(|b| b.point < p || (b.inclusive && b.point == p))
            && self
                .hi
                .is_none_or(|b| p < b.point || (b.inclusive && b.point == p))
    }

    /// Whether no value is in it.
    pub fn is_empty(&self) -> bool {
        match (self.lo, self.hi) {
            (Some(l), Some(h)) => {
                l.point > h.point || (l.point == h.point && !(l.inclusive && h.inclusive))
            }
            _ => false,
        }
    }

    /// Whether it holds no infinity of a date or timestamp.
    pub fn excludes_infinities(&self) -> bool {
        self.kind.datetime()
            && !self.contains(OrdPoint::NegInfinity)
            && !self.contains(OrdPoint::Infinity)
    }

    /// The smallest range holding both (`None` when they are of different
    /// types).
    pub(crate) fn hull(&self, other: &OrdRange) -> Option<OrdRange> {
        if self.kind != other.kind {
            return None;
        }
        let pick = |a: Option<OrdBound>, b: Option<OrdBound>, lower: bool| {
            let (a, b) = (a?, b?);
            Some(match a.point.cmp(&b.point) {
                Ordering::Equal => OrdBound {
                    point: a.point,
                    inclusive: a.inclusive || b.inclusive,
                },
                Ordering::Less => {
                    if lower {
                        a
                    } else {
                        b
                    }
                }
                Ordering::Greater => {
                    if lower {
                        b
                    } else {
                        a
                    }
                }
            })
        };
        Some(OrdRange {
            kind: self.kind,
            lo: pick(self.lo, other.lo, true),
            hi: pick(self.hi, other.hi, false),
        })
    }

    /// What lies in both (`self` when they are of different types).
    pub(crate) fn meet(&self, other: &OrdRange) -> OrdRange {
        if self.kind != other.kind {
            return *self;
        }
        let pick = |a: Option<OrdBound>, b: Option<OrdBound>, lower: bool| match (a, b) {
            (Some(a), Some(b)) => Some(match a.point.cmp(&b.point) {
                Ordering::Equal => OrdBound {
                    point: a.point,
                    inclusive: a.inclusive && b.inclusive,
                },
                Ordering::Less => {
                    if lower {
                        b
                    } else {
                        a
                    }
                }
                Ordering::Greater => {
                    if lower {
                        a
                    } else {
                        b
                    }
                }
            }),
            (a, b) => a.or(b),
        };
        OrdRange {
            kind: self.kind,
            lo: pick(self.lo, other.lo, true),
            hi: pick(self.hi, other.hi, false),
        }
    }

    /// The range of integers within `r`, as `numeric` values.
    pub(crate) fn of_ints(r: &crate::refine::IntRange) -> OrdRange {
        let at = |v: Option<i128>| {
            v.map(|v| OrdBound {
                point: OrdPoint::Finite(Num::int(v)),
                inclusive: true,
            })
        };
        OrdRange {
            kind: OrdKind::Numeric,
            lo: at(r.lo),
            hi: at(r.hi),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> Num {
        Num::parse(s).unwrap()
    }

    #[test]
    fn decimals_compare_by_value() {
        assert_eq!(n("1"), n("1.000"));
        assert_eq!(n("1e2"), n("100"));
        assert_eq!(n("-0.50"), n("-.5"));
        assert!(n("0.30000000000000001") > n("0.3"));
        assert!(n("-1.5") < n("-1"));
        assert!(n("-0.5") < n("0.5"));
        assert!(n("2") > n("1.99999"));
        assert_eq!(n("1.50").to_string(), "1.5");
        assert_eq!(n("-0.025").to_string(), "-0.025");
        assert_eq!(n("12e1").to_string(), "120");
        assert!(Num::parse("1_000").is_none());
        assert!(Num::parse("0x1F").is_none());
        assert!(Num::parse("1e").is_none());
    }

    #[test]
    fn special_numerics_sort_around_the_numbers() {
        let r = |s: &str| OrdKind::Numeric.read(s).unwrap();
        assert!(r("-Infinity") < r("-1e30"));
        assert!(r(" inf ") > r("1e30"));
        assert!(r("NaN") > r("Infinity"));
        let gt5 = OrdRange {
            kind: OrdKind::Numeric,
            lo: Some(OrdBound {
                point: r("5"),
                inclusive: false,
            }),
            hi: None,
        };
        assert!(gt5.contains(r("nan")) && gt5.contains(r("infinity")));
        assert!(!gt5.contains(r("-inf")) && !gt5.contains(r("5.0")));
    }

    #[test]
    fn datetimes_are_read_only_when_exact() {
        assert_eq!(
            OrdKind::Date.read("2000-01-02"),
            Some(OrdPoint::Finite(Num::int(1)))
        );
        assert_eq!(OrdKind::Date.read("-infinity"), Some(OrdPoint::NegInfinity));
        assert!(OrdKind::Date.read("today").is_none());
        assert!(OrdKind::Date.read("01/02/2000").is_none());
        assert_eq!(
            OrdKind::TimestampTz.read("2000-01-01 01:00:00+01"),
            Some(OrdPoint::Finite(Num::int(0)))
        );
        assert!(OrdKind::TimestampTz.read("2000-01-01 01:00:00").is_none());
        assert!(OrdKind::Timestamp.read("now").is_none());
        assert_eq!(
            OrdKind::Timestamp.read("2000-01-01 00:00:01"),
            Some(OrdPoint::Finite(Num::int(1_000_000)))
        );
    }
}
