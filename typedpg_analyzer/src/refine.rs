//! What is known of a value beyond its type and nullability.
//!
//! A [`Refinement`] describes every non-NULL value an expression yields (a
//! NULL satisfies any refinement). It is carried with the type the way
//! array element nullability is: from a table column, a literal or a
//! function that proves it, through column references, subqueries, CTEs,
//! set operations and conditional expressions, to the query's output. The
//! default knows nothing, and a rule only ever adds what it proves, so a
//! construct the analysis doesn't follow leaves its value unrefined.

use std::collections::BTreeSet;

use crate::oid::PgTypeOid;
use crate::pg_catalog::PgCatalog;

/// What every non-NULL value of an expression is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Refinement {
    /// Never `infinity` or `-infinity`: of a `date`, `timestamp`,
    /// `timestamptz` or `interval`, a finite one (`to_char` and `extract`
    /// of it aren't NULL). Of a literal of no type yet, that it doesn't
    /// spell an infinity in any type it may be read as.
    pub finite: bool,
    /// One of these, when they are few and known: of a type whose equal
    /// values print the same (see [`Exact`]), in that print — an integer
    /// in decimal, `true` / `false`, a string (an enum label) as is. Of a
    /// literal of no type yet, as written: [`Refinement::converted`] reads
    /// it as the type its context gives it.
    pub values: Option<BTreeSet<String>>,
    /// Of an integer, the bounds it lies within.
    pub range: Option<IntRange>,
}

/// The bounds an integer lies within, each inclusive (`None`: unbounded).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct IntRange {
    pub lo: Option<i128>,
    pub hi: Option<i128>,
}

impl IntRange {
    /// At least `lo`.
    pub(crate) const fn at_least(lo: i128) -> IntRange {
        IntRange {
            lo: Some(lo),
            hi: None,
        }
    }

    /// Exactly `v`.
    pub(crate) const fn exactly(v: i128) -> IntRange {
        IntRange {
            lo: Some(v),
            hi: Some(v),
        }
    }

    /// Whether `v` may be in it.
    pub(crate) fn contains(&self, v: i128) -> bool {
        self.lo.is_none_or(|lo| lo <= v) && self.hi.is_none_or(|hi| v <= hi)
    }

    /// The smallest range holding both.
    pub(crate) fn hull(&self, other: &IntRange) -> IntRange {
        IntRange {
            lo: self.lo.zip(other.lo).map(|(a, b)| a.min(b)),
            hi: self.hi.zip(other.hi).map(|(a, b)| a.max(b)),
        }
    }

    /// What lies in both.
    pub(crate) fn meet(&self, other: &IntRange) -> IntRange {
        let pick = |a: Option<i128>, b: Option<i128>, f: fn(i128, i128) -> i128| match (a, b) {
            (Some(a), Some(b)) => Some(f(a, b)),
            (a, b) => a.or(b),
        };
        IntRange {
            lo: pick(self.lo, other.lo, i128::max),
            hi: pick(self.hi, other.hi, i128::min),
        }
    }

    /// The range of `a + b`, `a - b`, `a * b` or `-a` (PG fails on an
    /// overflow rather than wrap, so the arithmetic is exact).
    pub(crate) fn add(&self, b: &IntRange) -> IntRange {
        IntRange {
            lo: self.lo.zip(b.lo).and_then(|(x, y)| x.checked_add(y)),
            hi: self.hi.zip(b.hi).and_then(|(x, y)| x.checked_add(y)),
        }
    }

    pub(crate) fn neg(&self) -> IntRange {
        IntRange {
            lo: self.hi.and_then(i128::checked_neg),
            hi: self.lo.and_then(i128::checked_neg),
        }
    }

    pub(crate) fn mul(&self, b: &IntRange) -> IntRange {
        let (Some(a0), Some(a1), Some(b0), Some(b1)) = (self.lo, self.hi, b.lo, b.hi) else {
            // Non-negative times non-negative stays non-negative.
            let non_negative = |r: &IntRange| r.lo.is_some_and(|lo| lo >= 0);
            return if non_negative(self) && non_negative(b) {
                IntRange::at_least(0)
            } else {
                IntRange::default()
            };
        };
        let products: Option<Vec<i128>> = [a0, a1]
            .iter()
            .flat_map(|x| [b0, b1].map(|y| x.checked_mul(y)))
            .collect();
        match products {
            Some(p) => IntRange {
                lo: p.iter().min().copied(),
                hi: p.iter().max().copied(),
            },
            None => IntRange::default(),
        }
    }
}

impl Refinement {
    /// Nothing known.
    pub const NONE: Refinement = Refinement {
        finite: false,
        values: None,
        range: None,
    };

    /// A finite value.
    pub const FINITE: Refinement = Refinement {
        finite: true,
        values: None,
        range: None,
    };

    /// The bounds an integer value lies within: its range, narrowed by its
    /// values when they are integers.
    pub fn int_bounds(&self) -> Option<IntRange> {
        let from_values = self.values.as_ref().and_then(|vs| {
            let ints: Option<Vec<i128>> = vs.iter().map(|v| v.parse().ok()).collect();
            let ints = ints?;
            Some(IntRange {
                lo: ints.iter().min().copied(),
                hi: ints.iter().max().copied(),
            })
        });
        match (self.range, from_values) {
            (Some(r), Some(v)) => Some(r.meet(&v)),
            (r, v) => r.or(v),
        }
    }

    /// Its values, or those of a range of at most [`ENUMERATED`] integers.
    pub fn enumerated(&self) -> Option<BTreeSet<String>> {
        if let Some(vs) = &self.values {
            return Some(vs.clone());
        }
        let IntRange {
            lo: Some(lo),
            hi: Some(hi),
        } = self.range?
        else {
            return None;
        };
        (hi >= lo && hi - lo < ENUMERATED).then(|| (lo..=hi).map(|v| v.to_string()).collect())
    }

    /// Of a value that is one of `parts` (CASE branches, UNION arms): what
    /// every one of them is. Nothing for no part.
    pub(crate) fn either<'a>(parts: impl IntoIterator<Item = &'a Refinement>) -> Refinement {
        let parts: Vec<&Refinement> = parts.into_iter().collect();
        if parts.is_empty() {
            return Refinement::NONE;
        }
        let values = parts
            .iter()
            .map(|p| p.values.as_ref())
            .collect::<Option<Vec<_>>>()
            .map(|sets| sets.into_iter().flatten().cloned().collect());
        let range = parts
            .iter()
            .map(|p| p.range)
            .collect::<Option<Vec<_>>>()
            .and_then(|rs| rs.into_iter().reduce(|a, b| a.hull(&b)));
        Refinement {
            finite: parts.iter().all(|p| p.finite),
            values,
            range,
        }
    }

    /// Of a value that is both (a column CHECKed `isfinite` whose domain
    /// says more): what either is.
    pub(crate) fn and(&self, other: &Refinement) -> Refinement {
        Refinement {
            finite: self.finite || other.finite,
            values: match (&self.values, &other.values) {
                (Some(a), Some(b)) => Some(a.intersection(b).cloned().collect()),
                (a, b) => a.clone().or_else(|| b.clone()),
            },
            range: match (self.range, other.range) {
                (Some(a), Some(b)) => Some(a.meet(&b)),
                (a, b) => a.or(b),
            },
        }
    }

    /// The refinement of a string literal (of type `unknown` until its
    /// context types it): finite when it doesn't spell an infinity —
    /// `infinity`, `-infinity`, `+infinity` and their case variants
    /// (`DecodeSpecial`), or a float's `inf` — and the text itself.
    pub(crate) fn of_string_literal(text: &str) -> Refinement {
        Refinement {
            finite: !text.to_ascii_lowercase().contains("inf"),
            values: Some(BTreeSet::from([text.to_owned()])),
            range: None,
        }
    }

    /// What is left of it once a value of type `from` is converted to type
    /// `to`: the same type (or domain of it), a pseudo-type (`anyelement`:
    /// no conversion), an untyped literal's input, or a conversion between
    /// `date`, `timestamp` and `timestamptz` (each maps an infinity to an
    /// infinity, and a finite value to a finite one or an error) keep it.
    pub(crate) fn converted(
        &self,
        from: PgTypeOid,
        to: PgTypeOid,
        snapshot: &PgCatalog,
    ) -> Refinement {
        use crate::pg_catalog::oid;
        let (from, to) = (snapshot.unwrap_domain(from), snapshot.unwrap_domain(to));
        let datetime = |t: PgTypeOid| [oid::DATE, oid::TIMESTAMP, oid::TIMESTAMPTZ].contains(&t);
        let pseudo = snapshot
            .pg_type
            .get(&to)
            .is_some_and(|t| t.typtype == crate::pg_catalog::TypType::Pseudo);
        let finite = self.finite
            && (from == to || pseudo || from == oid::UNKNOWN || (datetime(from) && datetime(to)));
        let values = match &self.values {
            Some(vs) if from == oid::UNKNOWN => Exact::of(to, None, snapshot)
                .and_then(|kind| vs.iter().map(|v| kind.read(v)).collect()),
            // An integer is the same number in any integer type.
            Some(vs)
                if from == to
                    || pseudo
                    || (Exact::of(from, None, snapshot) == Some(Exact::Int)
                        && Exact::of(to, None, snapshot) == Some(Exact::Int)) =>
            {
                Some(vs.clone())
            }
            _ => None,
        };
        // An integer is the same number in any integer type.
        let ints = Exact::of(from, None, snapshot) == Some(Exact::Int)
            && Exact::of(to, None, snapshot) == Some(Exact::Int);
        let range = self.range.filter(|_| from == to || pseudo || ints);
        Refinement {
            finite,
            values,
            range,
        }
    }
}

/// A type whose equal values print the same, so a value set says what
/// the client receives: the integer types, `boolean`, an enum, and `text`
/// / `varchar` under a deterministic collation. Not `numeric` (`1` and
/// `1.0` are equal), `char(n)` (padded) or a string type under a
/// nondeterministic collation (`'a'` may be stored `'A'`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exact {
    Int,
    Bool,
    Text,
}

impl Exact {
    /// How values of type `t` (under `collation`, when it is a column's)
    /// print, when they print as they compare.
    pub(crate) fn of(
        t: PgTypeOid,
        collation: Option<crate::oid::PgCollationOid>,
        snapshot: &PgCatalog,
    ) -> Option<Exact> {
        use crate::pg_catalog::oid;
        let base = snapshot.unwrap_domain(t);
        if [oid::INT2, oid::INT4, oid::INT8].contains(&base) {
            return Some(Exact::Int);
        }
        if base == oid::BOOL {
            return Some(Exact::Bool);
        }
        if snapshot
            .pg_type
            .get(&base)
            .is_some_and(|ty| ty.typtype == crate::pg_catalog::TypType::Enum)
        {
            return Some(Exact::Text);
        }
        // C (950), POSIX (951) and the database default (100) compare
        // bytes.
        let deterministic = collation.is_none_or(|c| matches!(c.get(), 100 | 950 | 951));
        ((base == oid::TEXT || base == oid::VARCHAR) && deterministic).then_some(Exact::Text)
    }

    /// A literal's text as this kind of type reads it, in print: what
    /// `int4in` / `boolin` / `textin` make of it (`None` when they reject
    /// it, the cast failing anyway).
    pub(crate) fn read(self, text: &str) -> Option<String> {
        match self {
            Exact::Int => {
                crate::literal_input::parse_pg_integer(text.trim()).map(|i| i.to_string())
            }
            Exact::Bool => read_bool(text).map(|b| b.to_string()),
            Exact::Text => Some(text.to_owned()),
        }
    }
}

/// `boolin` (bool.c, `parse_bool_with_len`): a case-insensitive prefix of
/// `true` / `false` / `yes` / `no`, `on`, a prefix of `off` of two or more
/// letters, `1` or `0`, surrounded by whitespace.
fn read_bool(text: &str) -> Option<bool> {
    let s = text.trim().to_ascii_lowercase();
    let prefix = |word: &str, min: usize| s.len() >= min && word.starts_with(s.as_str());
    if prefix("true", 1) || prefix("yes", 1) || s == "on" || s == "1" {
        Some(true)
    } else if prefix("false", 1) || prefix("no", 1) || prefix("off", 2) || s == "0" {
        Some(false)
    } else {
        None
    }
}

/// The most integers a range is listed as values for.
const ENUMERATED: i128 = 16;

/// PG's `interval` type.
const INTERVAL: PgTypeOid = PgTypeOid::from_raw(1186);

/// Whether a value of `t` is never an infinity: refined so, or of a type
/// that has none (only the date / time ones, the floats and `numeric` do —
/// and an untyped literal or parameter may be read as one of them).
pub(crate) fn finite_value(t: &crate::expr::ExprType, snapshot: &PgCatalog) -> bool {
    use crate::pg_catalog::oid;
    t.refine.finite
        || ![
            oid::DATE,
            oid::TIMESTAMP,
            oid::TIMESTAMPTZ,
            INTERVAL,
            oid::FLOAT4,
            oid::FLOAT8,
            oid::NUMERIC,
            oid::UNKNOWN,
        ]
        .contains(&snapshot.unwrap_domain(t.type_oid))
}

/// The `pg_catalog` functions (called, or run by an operator) whose result
/// is finite when no argument is an infinity: each maps finite dates,
/// timestamps and intervals to a finite one — or fails, out of range (PG
/// checks every result, `IS_VALID_TIMESTAMP` / `INTERVAL_NOT_FINITE`) —
/// or returns one of its arguments. The current time has no argument.
const FINITE_PRESERVING: &[&str] = &[
    // The current time.
    "now",
    "statement_timestamp",
    "transaction_timestamp",
    "clock_timestamp",
    // Truncation, time zones, conversions.
    "date_trunc",
    "date_bin",
    "timezone",
    "date",
    "timestamp",
    "timestamptz",
    "justify_days",
    "justify_hours",
    "justify_interval",
    "make_date",
    "make_timestamp",
    "make_timestamptz",
    "make_interval",
    "age",
    // Arithmetic (`+`, `-`, `*`, `/` on dates, timestamps and intervals).
    "date_pli",
    "date_mii",
    "date_pl_interval",
    "date_mi_interval",
    "datetime_pl",
    "datetimetz_pl",
    "timedate_pl",
    "timetzdate_pl",
    "interval_pl_date",
    "interval_pl_timestamp",
    "interval_pl_timestamptz",
    "timestamp_pl_interval",
    "timestamp_mi_interval",
    "timestamptz_pl_interval",
    "timestamptz_mi_interval",
    "timestamp_mi",
    "timestamptz_mi",
    "interval_pl",
    "interval_mi",
    "interval_um",
    "interval_mul",
    "mul_d_interval",
    "interval_div",
    // One of the arguments' values.
    "min",
    "max",
    "first_value",
    "last_value",
    "nth_value",
    "lag",
    "lead",
    "generate_series",
];

/// The refinement of the result of `pg_catalog` function `proc` over
/// arguments `args` (see [`FINITE_PRESERVING`]).
pub(crate) fn of_builtin_call(
    proc: &crate::pg_catalog::PgProc,
    args: &[&crate::expr::ExprType],
    snapshot: &PgCatalog,
) -> Refinement {
    let builtin = snapshot.namespace_name(proc.pronamespace) == Some("pg_catalog");
    // `to_timestamp(text, text)` parses; `to_timestamp(float8)` converts.
    let finite_preserving = FINITE_PRESERVING.contains(&proc.proname.as_str())
        || (proc.proname == "to_timestamp" && proc.proargtypes.len() == 1);
    Refinement {
        finite: builtin && finite_preserving && args.iter().all(|a| finite_value(a, snapshot)),
        values: None,
        range: builtin
            .then(|| builtin_range(proc, args, snapshot))
            .flatten(),
    }
}

/// The bounds of the integer result of `pg_catalog` function `proc` over
/// `args`: counts and positions are never negative (`row_number` starts
/// at 1), `min` / `max` / `lag` are an argument's value, and integer
/// arithmetic is exact (PG fails on an overflow rather than wrap).
fn builtin_range(
    proc: &crate::pg_catalog::PgProc,
    args: &[&crate::expr::ExprType],
    snapshot: &PgCatalog,
) -> Option<IntRange> {
    let bounds = |i: usize| -> IntRange {
        args.get(i)
            .and_then(|a| {
                (Exact::of(a.type_oid, None, snapshot) == Some(Exact::Int))
                    .then(|| a.refine.int_bounds())
                    .flatten()
            })
            .unwrap_or_default()
    };
    let int_result = Exact::of(proc.prorettype, None, snapshot) == Some(Exact::Int);
    let name = proc.proname.as_str();
    // `int4pl`, `int48mi`, `int2mul`, `int8um`, …
    let arith = name
        .strip_prefix("int")
        .and_then(|rest| {
            let digits = rest.trim_start_matches(['2', '4', '8']);
            (rest.len() - digits.len() <= 2 && rest.len() > digits.len()).then_some(digits)
        })
        .filter(|_| int_result);
    let range = match (name, arith) {
        (_, Some("pl")) => bounds(0).add(&bounds(1)),
        (_, Some("mi")) => bounds(0).add(&bounds(1).neg()),
        (_, Some("mul")) => bounds(0).mul(&bounds(1)),
        (_, Some("um")) => bounds(0).neg(),
        ("count", _) => IntRange::at_least(0),
        ("row_number" | "rank" | "dense_rank" | "ntile", _) => IntRange::at_least(1),
        (
            "length" | "char_length" | "character_length" | "octet_length" | "bit_length"
            | "cardinality" | "num_nulls" | "num_nonnulls" | "abs",
            _,
        ) if int_result => IntRange::at_least(0),
        ("array_length" | "array_ndims", _) => IntRange::at_least(1),
        ("sum", _) => {
            let b = bounds(0);
            IntRange {
                lo: b.lo.filter(|&lo| lo >= 0).map(|_| 0),
                hi: b.hi.filter(|&hi| hi <= 0).map(|_| 0),
            }
        }
        ("min" | "max" | "first_value" | "last_value" | "nth_value" | "lag" | "lead", _) => {
            let mut b = bounds(0);
            // `lag(x, n, default)` may be the default.
            if args.len() >= 3 {
                b = b.hull(&bounds(2));
            }
            b
        }
        _ => return None,
    };
    (range != IntRange::default()).then_some(range)
}
