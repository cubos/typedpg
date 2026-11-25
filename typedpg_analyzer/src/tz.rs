//! The session `TimeZone`, where the analyzer knows what PG's timezone
//! library makes of it: a fixed offset — a number of hours or an
//! `INTERVAL` (`check_timezone`, `pg_tzset_offset`), or a zone of the IANA
//! database with one offset at every instant (`UTC`, `Etc/GMT+3`, …). A
//! zone whose offset changed over time (`America/Sao_Paulo`) would need
//! the server's tz database: what it reads or prints isn't known here.

/// A time zone of one UTC offset.
#[derive(Clone, Debug)]
pub(crate) struct Zone {
    /// Seconds east of UTC.
    east: i32,
    /// The abbreviation PG prints for it (`UTC`, `-03`, `+05:45`).
    abbrev: String,
}

/// The session's `TimeZone` setting as `SET` gave it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TzSetting {
    /// The server's: taken as UTC (see `PgCatalog::timezone_utc`).
    Default,
    /// A zone name or POSIX specification, or a number of hours.
    Text(String),
    /// `INTERVAL '…'`: seconds east of UTC.
    Interval(i64),
    /// Not known here.
    Unknown,
}

impl Zone {
    /// The zone `setting` loads, when it has one offset known here.
    pub(crate) fn of(setting: &TzSetting) -> Option<Zone> {
        match setting {
            TzSetting::Default => Some(Zone {
                east: 0,
                abbrev: "UTC".to_owned(),
            }),
            TzSetting::Interval(east) => Zone::offset(i32::try_from(*east).ok()?),
            TzSetting::Text(text) => {
                let text = text.trim();
                if let Ok(hours) = text.parse::<f64>() {
                    if !hours.is_finite() {
                        return None;
                    }
                    // check_timezone: SQL's sign convention.
                    return Zone::offset(i32::try_from((hours * 3600.0) as i64).ok()?);
                }
                Zone::fixed_named(text)
            }
            TzSetting::Unknown => None,
        }
    }

    /// `pg_tzset_offset`: `east` seconds east of UTC, abbreviated by its
    /// offset (`<-03>+03` prints as `-03`).
    fn offset(east: i32) -> Option<Zone> {
        // Beyond the POSIX limit pg_tzset_offset fails.
        if east.abs() >= 168 * 3600 {
            return None;
        }
        Some(Zone {
            east,
            abbrev: offset_abbrev(east),
        })
    }

    /// The zones of the IANA database with a single offset ever — the UTC
    /// aliases and `Etc/GMT±N` (whose sign is POSIX's: `Etc/GMT+3` is three
    /// hours west, abbreviated `-03`) — matched case-insensitively.
    fn fixed_named(name: &str) -> Option<Zone> {
        let lower = name.to_ascii_lowercase();
        let lower = lower.strip_prefix("etc/").unwrap_or(&lower);
        let utc = |abbrev: &str| {
            Some(Zone {
                east: 0,
                abbrev: abbrev.to_owned(),
            })
        };
        match lower {
            "utc" | "uct" | "universal" | "zulu" => utc("UTC"),
            "gmt" | "gmt0" | "gmt+0" | "gmt-0" | "greenwich" => utc("GMT"),
            _ => {
                // Only `Etc/GMT±N` (N ≤ 14 east, ≤ 12 west), not the bare
                // `GMT+N` POSIX specification.
                if !name.to_ascii_lowercase().starts_with("etc/") {
                    return None;
                }
                let rest = lower.strip_prefix("gmt")?;
                let (sign, digits) = rest.split_at_checked(1)?;
                if digits.is_empty()
                    || digits.len() > 2
                    || !digits.bytes().all(|b| b.is_ascii_digit())
                {
                    return None;
                }
                let hours: i32 = digits.parse().ok()?;
                let east = match sign {
                    "+" if hours <= 12 => -hours * 3600,
                    "-" if hours <= 14 => hours * 3600,
                    _ => return None,
                };
                Some(Zone {
                    east,
                    abbrev: offset_abbrev(east),
                })
            }
        }
    }

    /// The offset (seconds east) and abbreviation in effect at any instant.
    pub(crate) fn at(&self) -> (i32, &str) {
        (self.east, &self.abbrev)
    }

    /// DetermineTimeZoneOffset: the offset (seconds east) of a local time.
    pub(crate) fn local_offset(&self) -> i32 {
        self.east
    }
}

/// `-03`, `+05:30`, `+05:30:15`: an offset as pg_tzset_offset names it.
fn offset_abbrev(east: i32) -> String {
    let abs = east.unsigned_abs();
    let mut abbrev = format!("{}{:02}", if east >= 0 { '+' } else { '-' }, abs / 3600);
    if !abs.is_multiple_of(3600) {
        abbrev.push_str(&format!(":{:02}", abs % 3600 / 60));
        if !abs.is_multiple_of(60) {
            abbrev.push_str(&format!(":{:02}", abs % 60));
        }
    }
    abbrev
}
