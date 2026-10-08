//! What is known of a value beyond its type and nullability.
//!
//! A [`Refinement`] describes every non-NULL value an expression yields (a
//! NULL satisfies any refinement). It is carried with the type the way
//! array element nullability is: from a table column, a literal or a
//! function that proves it, through column references, subqueries, CTEs,
//! set operations and conditional expressions, to the query's output. The
//! default knows nothing, and a rule only ever adds what it proves, so a
//! construct the analysis doesn't follow leaves its value unrefined.

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
}

impl Refinement {
    /// Nothing known.
    pub const NONE: Refinement = Refinement { finite: false };

    /// A finite value.
    pub const FINITE: Refinement = Refinement { finite: true };

    /// Of a value that is one of `parts` (CASE branches, UNION arms): what
    /// every one of them is. Nothing for no part.
    pub(crate) fn either<'a>(parts: impl IntoIterator<Item = &'a Refinement>) -> Refinement {
        let mut parts = parts.into_iter().peekable();
        if parts.peek().is_none() {
            return Refinement::NONE;
        }
        Refinement {
            finite: parts.all(|p| p.finite),
        }
    }

    /// Of a value that is both (a column CHECKed `isfinite` whose domain
    /// says more): what either is.
    pub(crate) fn and(&self, other: &Refinement) -> Refinement {
        Refinement {
            finite: self.finite || other.finite,
        }
    }

    /// The refinement of a string literal (of type `unknown` until its
    /// context types it): finite when it doesn't spell an infinity —
    /// `infinity`, `-infinity`, `+infinity` and their case variants
    /// (`DecodeSpecial`), or a float's `inf`.
    pub(crate) fn of_string_literal(text: &str) -> Refinement {
        Refinement {
            finite: !text.to_ascii_lowercase().contains("inf"),
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
        if from == to || pseudo || from == oid::UNKNOWN || (datetime(from) && datetime(to)) {
            self.clone()
        } else {
            Refinement::NONE
        }
    }
}

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
    }
}
