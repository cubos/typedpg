//! The datetime family's constants, which PG keeps as values read under
//! the session's settings when the expression was written (DateOrder,
//! TimeZone, IntervalStyle) and prints under the settings of the moment
//! (DateStyle, TimeZone, IntervalStyle). A stored literal is rewritten to
//! its *canonical* text when written ([`Style::Canonical`]: ISO, UTC,
//! ISO 8601 intervals — read the same under every setting), so that it
//! keeps meaning the value PG stored; [`output`] prints a value the way
//! the output function does.

use crate::datetime_input::{self as di, DateOrder, DatetimeFields, IntervalValue, LiteralZone};
use crate::ddl::session::{DateStyle, IntervalStyle};
use crate::pg_catalog::PgCatalog;
use crate::tz::{TzSetting, Zone};

/// The text wanted of a constant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Style {
    /// The output function's, under the session's settings.
    Session,
    /// The text that reads as the same value under every setting.
    Canonical,
}

/// The session settings a datetime constant is read and printed under.
pub(crate) struct Settings {
    date: DateStyle,
    order: DateOrder,
    interval: IntervalStyle,
    timezone: TzSetting,
}

impl Settings {
    pub(crate) fn of(interp: &PgCatalog) -> Settings {
        let (date, order) = interp.datestyle.get();
        Settings {
            date,
            order,
            interval: interp.intervalstyle.get(),
            timezone: interp.timezone.get(),
        }
    }

    /// The zone values are printed in: the server's, when no `SET` gave
    /// one, taken as UTC (`PgCatalog::timezone_utc`).
    fn zone(&self) -> Option<Zone> {
        Zone::of(&self.timezone)
    }

    /// The zone a literal naming none is read in. The server's own isn't
    /// known: a value read in it isn't either (a claim made of it, a
    /// refinement's bound, would be unsound wherever it isn't UTC).
    fn input_zone(&self) -> Option<Zone> {
        match self.timezone {
            TzSetting::Default => None,
            _ => self.zone(),
        }
    }
}

/// Whether the constants of the built-in type `typname` depend on the
/// session's settings.
pub(crate) fn is_settings_dependent(typname: &str) -> bool {
    matches!(
        typname,
        "date" | "timestamp" | "timestamptz" | "time" | "timetz" | "interval"
    )
}

/// What the output function of the built-in datetime type `typname`
/// prints for literal `v`, read under the session's settings (or its
/// canonical text); `None` where that isn't known here.
pub(crate) fn output(typname: &str, v: &str, s: &Settings, style: Style) -> Option<String> {
    let session = style == Style::Session;
    let (date, order) = if session {
        (s.date, s.order)
    } else {
        (DateStyle::Iso, DateOrder::Mdy)
    };
    match typname {
        "date" => date_text(date_value(v, s)?, date, order),
        "timestamp" => datetime_text(timestamp_value(v, s)?, None, date, order),
        "timestamptz" => timestamptz_text(timestamptz_value(v, s)?, s, style),
        "time" => Some(time_text(time_value(v, s)?)),
        "timetz" => {
            let (usec, west) = timetz_value(v, s)?;
            Some(format!("{}{}", time_text(usec), zone_offset_text(west)))
        }
        "interval" => interval_text(
            interval_value(v, s)?,
            if session {
                s.interval
            } else {
                IntervalStyle::Iso8601
            },
        ),
        _ => None,
    }
}

/// The output of the `date` (days), `timestamp` or `timestamptz`
/// (microseconds) value `value` — of a range's bound.
pub(crate) fn value_text(typname: &str, value: i128, s: &Settings, style: Style) -> Option<String> {
    let (date, order) = match style {
        Style::Session => (s.date, s.order),
        Style::Canonical => (DateStyle::Iso, DateOrder::Mdy),
    };
    match typname {
        "date" => date_text(value, date, order),
        "timestamp" => datetime_text(value, None, date, order),
        "timestamptz" => timestamptz_text(value, s, style),
        _ => None,
    }
}

/// `date_in`: days since 2000-01-01 (`±infinity` as the extremes).
pub(crate) fn date_value(v: &str, s: &Settings) -> Option<i128> {
    match di::datetime_fields(v, false, s.order)? {
        DatetimeFields::Infinity => Some(i128::MAX),
        DatetimeFields::NegInfinity => Some(i128::MIN),
        DatetimeFields::Epoch => Some(-10_957),
        DatetimeFields::Value { days, .. } => di::date_in_range(days).then_some(i128::from(days)),
    }
}

const USECS_PER_DAY: i128 = 86_400_000_000;
/// 2000-01-01 in seconds since 1970.
const POSTGRES_EPOCH_UNIX: i128 = 946_684_800;

/// `timestamp_in`: microseconds since 2000-01-01 (a zone is ignored).
pub(crate) fn timestamp_value(v: &str, s: &Settings) -> Option<i128> {
    match di::datetime_fields(v, true, s.order)? {
        DatetimeFields::Infinity => Some(i128::MAX),
        DatetimeFields::NegInfinity => Some(i128::MIN),
        DatetimeFields::Epoch => Some(-POSTGRES_EPOCH_UNIX * 1_000_000),
        DatetimeFields::Value { days, usec, .. } => {
            let t = i128::from(days) * USECS_PER_DAY + i128::from(usec);
            di::timestamp_in_range(t).then_some(t)
        }
    }
}

/// `timestamptz_in`: microseconds since 2000-01-01 UTC.
pub(crate) fn timestamptz_value(v: &str, s: &Settings) -> Option<i128> {
    match di::datetime_fields(v, true, s.order)? {
        DatetimeFields::Infinity => Some(i128::MAX),
        DatetimeFields::NegInfinity => Some(i128::MIN),
        DatetimeFields::Epoch => Some(-POSTGRES_EPOCH_UNIX * 1_000_000),
        DatetimeFields::Value { days, usec, zone } => {
            let local = i128::from(days) * USECS_PER_DAY + i128::from(usec);
            let east = match zone {
                LiteralZone::Offset(west) => -west,
                LiteralZone::Named(name) => Zone::of(&TzSetting::Text(name))?.local_offset(),
                LiteralZone::Session => s.input_zone()?.local_offset(),
            };
            let t = local - i128::from(east) * 1_000_000;
            di::timestamp_in_range(t).then_some(t)
        }
    }
}

/// `time_in`: microseconds since midnight (a zone is ignored).
pub(crate) fn time_value(v: &str, s: &Settings) -> Option<i64> {
    Some(di::time_fields(v, s.order)?.usec)
}

/// `timetz_in`: microseconds since midnight and the zone offset (seconds
/// west). Without one, the session's zone (of one offset: a zone whose
/// offset changed isn't known here).
pub(crate) fn timetz_value(v: &str, s: &Settings) -> Option<(i64, i32)> {
    let t = di::time_fields(v, s.order)?;
    let zone = match t.zone {
        LiteralZone::Offset(west) => return Some((t.usec, west)),
        LiteralZone::Named(name) => Zone::of(&TzSetting::Text(name))?,
        LiteralZone::Session => s.input_zone()?,
    };
    Some((t.usec, -zone.local_offset()))
}

/// `interval_in` under the session's IntervalStyle.
pub(crate) fn interval_value(v: &str, s: &Settings) -> Option<IntervalValue> {
    match s.interval {
        IntervalStyle::Unknown => None,
        style => di::interval_value(v, style == IntervalStyle::SqlStandard),
    }
}

/// A year as EncodeDateOnly writes it: at least four digits, the BC
/// years counted from 1.
fn year_text(y: i32) -> String {
    format!("{:04}", if y > 0 { y } else { -(y - 1) })
}

/// The civil date of `days` since 2000-01-01.
fn civil(days: i128) -> Option<(i32, i32, i32)> {
    Some(di::j2date(i32::try_from(days + 2_451_545).ok()?))
}

/// `date_out` (EncodeDateOnly) of `days` since 2000-01-01.
pub(crate) fn date_text(days: i128, style: DateStyle, order: DateOrder) -> Option<String> {
    match days {
        i128::MAX => return Some("infinity".to_owned()),
        i128::MIN => return Some("-infinity".to_owned()),
        _ => {}
    }
    let (y, m, d) = civil(days)?;
    let year = year_text(y);
    let dmy = order == DateOrder::Dmy;
    let mut out = match style {
        DateStyle::Iso => format!("{year}-{m:02}-{d:02}"),
        DateStyle::Sql if dmy => format!("{d:02}/{m:02}/{year}"),
        DateStyle::Sql => format!("{m:02}/{d:02}/{year}"),
        DateStyle::German => format!("{d:02}.{m:02}.{year}"),
        DateStyle::Postgres if dmy => format!("{d:02}-{m:02}-{year}"),
        DateStyle::Postgres => format!("{m:02}-{d:02}-{year}"),
        DateStyle::Unknown => return None,
    };
    if y <= 0 {
        out.push_str(" BC");
    }
    Some(out)
}

/// AppendSeconds with `fillzeros`: the seconds on two digits, then the
/// fraction without its trailing zeros.
pub(crate) fn seconds_text(sec: i64, usec: i64, fill: bool) -> String {
    let mut out = if fill {
        format!("{:02}", sec.abs())
    } else {
        sec.abs().to_string()
    };
    if usec != 0 {
        let frac = format!("{:06}", usec.abs());
        out.push('.');
        out.push_str(frac.trim_end_matches('0'));
    }
    out
}

/// EncodeTimezone (any style but XSD): `tz` seconds west as `+hh`,
/// `+hh:mm` or `+hh:mm:ss`.
fn zone_offset_text(tz: i32) -> String {
    let abs = tz.abs();
    let (h, m, s) = (abs / 3600, abs / 60 % 60, abs % 60);
    let sign = if tz <= 0 { '+' } else { '-' };
    if s != 0 {
        format!("{sign}{h:02}:{m:02}:{s:02}")
    } else if m != 0 {
        format!("{sign}{h:02}:{m:02}")
    } else {
        format!("{sign}{h:02}")
    }
}

/// `time_out` (EncodeTimeOnly) of microseconds since midnight.
fn time_text(usec: i64) -> String {
    let (hour, rest) = (usec / 3_600_000_000, usec % 3_600_000_000);
    let (min, rest) = (rest / 60_000_000, rest % 60_000_000);
    format!(
        "{hour:02}:{min:02}:{}",
        seconds_text(rest / 1_000_000, rest % 1_000_000, true)
    )
}

const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// EncodeDateTime of the local time `t` (microseconds since 2000-01-01),
/// with its zone (seconds west, abbreviation) for a `timestamptz`.
fn datetime_text(
    t: i128,
    tz: Option<(i32, &str)>,
    style: DateStyle,
    order: DateOrder,
) -> Option<String> {
    match t {
        i128::MAX => return Some("infinity".to_owned()),
        i128::MIN => return Some("-infinity".to_owned()),
        _ => {}
    }
    let days = t.div_euclid(USECS_PER_DAY);
    let time = i64::try_from(t.rem_euclid(USECS_PER_DAY)).ok()?;
    let (y, m, d) = civil(days)?;
    let year = year_text(y);
    let clock = time_text(time);
    let dmy = order == DateOrder::Dmy;
    let mut out = match style {
        DateStyle::Iso => format!("{year}-{m:02}-{d:02} {clock}"),
        DateStyle::Sql if dmy => format!("{d:02}/{m:02}/{year} {clock}"),
        DateStyle::Sql => format!("{m:02}/{d:02}/{year} {clock}"),
        DateStyle::German => format!("{d:02}.{m:02}.{year} {clock}"),
        DateStyle::Postgres => {
            // j2day: the Julian day's weekday, Sunday first.
            let wday = (days + 2_451_545 + 1).rem_euclid(7) as usize;
            let month = MONTHS[(m - 1) as usize];
            if dmy {
                format!("{} {d:02} {month} {clock} {year}", DAYS[wday])
            } else {
                format!("{} {month} {d:02} {clock} {year}", DAYS[wday])
            }
        }
        DateStyle::Unknown => return None,
    };
    if let Some((west, abbrev)) = tz {
        match style {
            DateStyle::Iso => out.push_str(&zone_offset_text(west)),
            _ => {
                out.push(' ');
                out.push_str(abbrev);
            }
        }
    }
    if y <= 0 {
        out.push_str(" BC");
    }
    Some(out)
}

/// `timestamptz_out` of `t` (microseconds since 2000-01-01 UTC): in the
/// session's zone, or at UTC for the canonical text.
fn timestamptz_text(t: i128, s: &Settings, style: Style) -> Option<String> {
    if t == i128::MAX || t == i128::MIN || style == Style::Canonical {
        return datetime_text(t, Some((0, "UTC")), DateStyle::Iso, DateOrder::Mdy);
    }
    let zone = s.zone()?;
    let (east, abbrev) = zone.at();
    datetime_text(
        t + i128::from(east) * 1_000_000,
        Some((-east, abbrev)),
        s.date,
        s.order,
    )
}

/// `interval_out` (EncodeInterval) in IntervalStyle `style`.
pub(crate) fn interval_text(iv: IntervalValue, style: IntervalStyle) -> Option<String> {
    let (months, days, usec) = match iv {
        IntervalValue::Infinity => return Some("infinity".to_owned()),
        IntervalValue::NegInfinity => return Some("-infinity".to_owned()),
        IntervalValue::Finite(m, d, t) => (m, d, t),
    };
    // interval2itm.
    let (year, mon) = (i64::from(months / 12), i64::from(months % 12));
    let mday = i64::from(days);
    let hour = usec / 3_600_000_000;
    let rest = usec - hour * 3_600_000_000;
    let min = rest / 60_000_000;
    let rest = rest - min * 60_000_000;
    let sec = rest / 1_000_000;
    let fsec = rest - sec * 1_000_000;
    let mut out = String::new();
    match style {
        IntervalStyle::Postgres => {
            let (mut is_zero, mut is_before) = (true, false);
            let mut part = |out: &mut String, value: i64, unit: &str| {
                if value == 0 {
                    return;
                }
                out.push_str(&format!(
                    "{}{}{value} {unit}{}",
                    if is_zero { "" } else { " " },
                    if is_before && value > 0 { "+" } else { "" },
                    if value != 1 { "s" } else { "" }
                ));
                is_before = value < 0;
                is_zero = false;
            };
            part(&mut out, year, "year");
            part(&mut out, mon, "mon");
            part(&mut out, mday, "day");
            if is_zero || hour != 0 || min != 0 || sec != 0 || fsec != 0 {
                let minus = hour < 0 || min < 0 || sec < 0 || fsec < 0;
                out.push_str(&format!(
                    "{}{}{:02}:{:02}:{}",
                    if is_zero { "" } else { " " },
                    if minus {
                        "-"
                    } else if is_before {
                        "+"
                    } else {
                        ""
                    },
                    hour.abs(),
                    min.abs(),
                    seconds_text(sec, fsec, true)
                ));
            }
        }
        IntervalStyle::SqlStandard => {
            let (mut year, mut mon, mut mday, mut hour, mut min, mut sec, mut fsec) =
                (year, mon, mday, hour, min, sec, fsec);
            let vals = [year, mon, mday, hour, min, sec, fsec];
            let has_negative = vals.iter().any(|v| *v < 0);
            let has_positive = vals.iter().any(|v| *v > 0);
            let has_year_month = year != 0 || mon != 0;
            let has_day_time = mday != 0 || hour != 0 || min != 0 || sec != 0 || fsec != 0;
            let sql_standard_value =
                !(has_negative && has_positive) && !(has_year_month && has_day_time);
            if has_negative && sql_standard_value {
                out.push('-');
                (year, mon, mday, hour, min, sec, fsec) =
                    (-year, -mon, -mday, -hour, -min, -sec, -fsec);
            }
            if !has_negative && !has_positive {
                out.push('0');
            } else if !sql_standard_value {
                let year_sign = if year < 0 || mon < 0 { '-' } else { '+' };
                let day_sign = if mday < 0 { '-' } else { '+' };
                let sec_sign = if hour < 0 || min < 0 || sec < 0 || fsec < 0 {
                    '-'
                } else {
                    '+'
                };
                out.push_str(&format!(
                    "{year_sign}{}-{} {day_sign}{} {sec_sign}{}:{:02}:{}",
                    year.abs(),
                    mon.abs(),
                    mday.abs(),
                    hour.abs(),
                    min.abs(),
                    seconds_text(sec, fsec, true)
                ));
            } else if has_year_month {
                out.push_str(&format!("{year}-{mon}"));
            } else if mday != 0 {
                out.push_str(&format!(
                    "{mday} {hour}:{min:02}:{}",
                    seconds_text(sec, fsec, true)
                ));
            } else {
                out.push_str(&format!(
                    "{hour}:{min:02}:{}",
                    seconds_text(sec, fsec, true)
                ));
            }
        }
        IntervalStyle::Iso8601 => {
            if [year, mon, mday, hour, min, sec, fsec]
                .iter()
                .all(|v| *v == 0)
            {
                return Some("PT0S".to_owned());
            }
            out.push('P');
            let part = |out: &mut String, v: i64, unit: char| {
                if v != 0 {
                    out.push_str(&format!("{v}{unit}"));
                }
            };
            part(&mut out, year, 'Y');
            part(&mut out, mon, 'M');
            part(&mut out, mday, 'D');
            if hour != 0 || min != 0 || sec != 0 || fsec != 0 {
                out.push('T');
            }
            part(&mut out, hour, 'H');
            part(&mut out, min, 'M');
            if sec != 0 || fsec != 0 {
                if sec < 0 || fsec < 0 {
                    out.push('-');
                }
                out.push_str(&seconds_text(sec, fsec, false));
                out.push('S');
            }
        }
        IntervalStyle::PostgresVerbose => {
            out.push('@');
            let (mut is_zero, mut is_before) = (true, false);
            let mut part = |out: &mut String, value: i64, unit: &str| {
                if value == 0 {
                    return;
                }
                let mut value = value;
                if is_zero {
                    is_before = value < 0;
                    value = value.abs();
                } else if is_before {
                    value = -value;
                }
                out.push_str(&format!(
                    " {value} {unit}{}",
                    if value == 1 { "" } else { "s" }
                ));
                is_zero = false;
            };
            part(&mut out, year, "year");
            part(&mut out, mon, "mon");
            part(&mut out, mday, "day");
            part(&mut out, hour, "hour");
            part(&mut out, min, "min");
            if sec != 0 || fsec != 0 {
                out.push(' ');
                if sec < 0 || (sec == 0 && fsec < 0) {
                    if is_zero {
                        is_before = true;
                    } else if !is_before {
                        out.push('-');
                    }
                } else if is_before {
                    out.push('-');
                }
                out.push_str(&seconds_text(sec, fsec, false));
                out.push_str(if sec.abs() != 1 || fsec != 0 {
                    " secs"
                } else {
                    " sec"
                });
                is_zero = false;
            }
            if is_zero {
                out.push_str(" 0");
            }
            if is_before {
                out.push_str(" ago");
            }
        }
        IntervalStyle::Unknown => return None,
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_print_in_every_style() {
        // interval_out on PostgreSQL 19, under each IntervalStyle.
        let cases = [
            ((0, 0, 0), ["00:00:00", "0", "PT0S", "@ 0"]),
            (
                (14, 3, 0),
                [
                    "1 year 2 mons 3 days",
                    "+1-2 +3 +0:00:00",
                    "P1Y2M3D",
                    "@ 1 year 2 mons 3 days",
                ],
            ),
            (
                (0, 1, 7_200_000_000),
                ["1 day 02:00:00", "1 2:00:00", "P1DT2H", "@ 1 day 2 hours"],
            ),
            (
                (0, -1, 7_200_000_000),
                [
                    "-1 days +02:00:00",
                    "+0-0 -1 +2:00:00",
                    "P-1DT2H",
                    "@ 1 day -2 hours ago",
                ],
            ),
            (
                (-1, 0, -1_500_000),
                [
                    "-1 mons -00:00:01.5",
                    "-0-1 +0 -0:00:01.5",
                    "P-1MT-1.5S",
                    "@ 1 mon 1.5 secs ago",
                ],
            ),
        ];
        for ((m, d, t), outs) in cases {
            for (style, out) in [
                IntervalStyle::Postgres,
                IntervalStyle::SqlStandard,
                IntervalStyle::Iso8601,
                IntervalStyle::PostgresVerbose,
            ]
            .into_iter()
            .zip(outs)
            {
                assert_eq!(
                    interval_text(IntervalValue::Finite(m, d, t), style).as_deref(),
                    Some(out),
                    "{style:?}"
                );
            }
        }
    }
}
