//! Exact decimal arithmetic for reasoning about integer and `numeric`
//! constants the way PostgreSQL computes them.

/// An exact decimal, `m × 10^-scale`: an integer or `numeric` value, whose
/// `+`, `-`, `*` and comparisons PG computes exactly — `0.1 + 0.2 = 0.3`
/// is TRUE, `0.30000000000000001 > 0.3` too (neither holds of `f64`s). A
/// step leaving `i128` gives up (an integer overflow is an error in PG
/// anyway, a numeric one would need more digits).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Decimal {
    m: i128,
    scale: u32,
}

impl Decimal {
    pub(crate) const ZERO: Decimal = Decimal { m: 0, scale: 0 };

    pub(crate) fn int(i: i64) -> Decimal {
        Decimal {
            m: i128::from(i),
            scale: 0,
        }
    }

    /// A numeric literal as the lexer leaves it (`1.5`, `.5`, `1e3`,
    /// `-2.50`, `9007199254740993`); `None` for the rest (hexadecimal,
    /// digits with underscores — not worth reading).
    pub(crate) fn parse(s: &str) -> Option<Decimal> {
        let (neg, s) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s.strip_prefix('+').unwrap_or(s)),
        };
        let (mantissa, exp) = match s.find(['e', 'E']) {
            Some(i) => (&s[..i], s[i + 1..].parse::<i64>().ok()?),
            None => (s, 0),
        };
        let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
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
        Some(Decimal {
            m: if neg { -m } else { m },
            scale: u32::try_from(scale).ok()?,
        })
    }

    /// Both mantissas at the larger scale.
    fn align(self, other: Decimal) -> Option<(i128, i128, u32)> {
        let scale = self.scale.max(other.scale);
        let at = |d: Decimal| d.m.checked_mul(10i128.checked_pow(scale - d.scale)?);
        Some((at(self)?, at(other)?, scale))
    }

    pub(crate) fn checked_add(self, other: Decimal) -> Option<Decimal> {
        let (a, b, scale) = self.align(other)?;
        Some(Decimal {
            m: a.checked_add(b)?,
            scale,
        })
    }

    pub(crate) fn checked_sub(self, other: Decimal) -> Option<Decimal> {
        self.checked_add(other.checked_neg()?)
    }

    pub(crate) fn checked_mul(self, other: Decimal) -> Option<Decimal> {
        Some(Decimal {
            m: self.m.checked_mul(other.m)?,
            scale: self.scale.checked_add(other.scale)?,
        })
    }

    pub(crate) fn checked_neg(self) -> Option<Decimal> {
        Some(Decimal {
            m: self.m.checked_neg()?,
            scale: self.scale,
        })
    }

    pub(crate) fn compare(self, other: Decimal) -> Option<std::cmp::Ordering> {
        let (a, b, _) = self.align(other)?;
        Some(a.cmp(&b))
    }

    /// The nearest integer, halves away from zero — PG's numeric to
    /// integer cast (`numericvar_to_int64` rounds; an integer is unchanged).
    pub(crate) fn round(self) -> Option<Decimal> {
        let unit = 10i128.checked_pow(self.scale)?;
        let (q, r) = (self.m / unit, self.m % unit);
        let q = if r.unsigned_abs() * 2 >= unit.unsigned_abs() {
            q + self.m.signum()
        } else {
            q
        };
        Some(Decimal { m: q, scale: 0 })
    }
}
