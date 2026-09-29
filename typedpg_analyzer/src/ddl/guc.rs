//! Configuration parameters set by migrations (`SET`, `RESET`,
//! `set_config`): PG knows every parameter a stock server has
//! (`pg_settings`, in the seed), refuses unknown names, parameters that
//! can't change in a session, and values of the wrong type or out of range
//! (`set_config_with_handle`, `parse_and_validate_value`).

use super::DdlError;
use crate::pg_catalog::{PgCatalog, PgSetting};

fn find<'a>(interp: &'a PgCatalog, name: &str) -> Option<&'a PgSetting> {
    interp
        .pg_settings
        .iter()
        .find(|s| s.name.eq_ignore_ascii_case(name))
}

/// Whether `name` is a parameter a GRANT ... ON PARAMETER may name
/// (check_GUC_name_for_parameter_acl): a known one, or a custom
/// `prefix.name` one.
pub(crate) fn exists(interp: &PgCatalog, name: &str) -> bool {
    find(interp, name).is_some() || name.contains('.')
}

fn invalid(err: String) -> Result<(), DdlError> {
    Err(DdlError::UnsupportedDdl(err))
}

/// The parameter must exist (or be a custom `prefix.name` one) and be
/// changeable in a session. Returns it when known.
pub(crate) fn check_settable<'a>(
    interp: &'a PgCatalog,
    name: &str,
) -> Result<Option<&'a PgSetting>, DdlError> {
    let Some(setting) = find(interp, name) else {
        if name.contains('.') {
            return Ok(None);
        }
        return Err(DdlError::TypeNotFound(format!(
            "unrecognized configuration parameter \"{name}\""
        )));
    };
    let n = &setting.name;
    let refused = match setting.context.as_str() {
        "internal" => Some(format!("parameter \"{n}\" cannot be changed")),
        "postmaster" => Some(format!(
            "parameter \"{n}\" cannot be changed without restarting the server"
        )),
        "sighup" => Some(format!("parameter \"{n}\" cannot be changed now")),
        "backend" | "superuser-backend" => Some(format!(
            "parameter \"{n}\" cannot be set after connection start"
        )),
        _ => None,
    };
    if let Some(msg) = refused {
        return Err(DdlError::UnsupportedDdl(msg));
    }
    Ok(Some(setting))
}

/// parse_and_validate_value for a new value of `setting`.
pub(crate) fn check_value(
    interp: &PgCatalog,
    setting: &PgSetting,
    value: &str,
) -> Result<(), DdlError> {
    let name = &setting.name;
    let bad_value = || format!("invalid value for parameter \"{name}\": \"{value}\"");
    match setting.vartype.as_str() {
        "bool" => {
            if super::reloptions::parse_bool(value).is_none() {
                return invalid(format!("parameter \"{name}\" requires a Boolean value"));
            }
        }
        "integer" | "real" => {
            let Some(number) = parse_with_unit(value, &setting.unit) else {
                return invalid(bad_value());
            };
            let integer = setting.vartype == "integer";
            let number = if integer { number.round() } else { number };
            if integer && (number > f64::from(i32::MAX) || number < f64::from(i32::MIN)) {
                return invalid(format!("{} (Value exceeds integer range.)", bad_value()));
            }
            let (Ok(min), Ok(max)) = (
                setting.min_val.parse::<f64>(),
                setting.max_val.parse::<f64>(),
            ) else {
                return Ok(());
            };
            if number < min || number > max {
                let unit = if setting.unit.is_empty() {
                    String::new()
                } else {
                    format!(" {}", setting.unit)
                };
                let show = |v: f64| {
                    if integer {
                        format!("{}", v as i64)
                    } else {
                        format_g(v)
                    }
                };
                return invalid(format!(
                    "{}{unit} is outside the valid range for parameter \"{name}\" ({}{unit} .. \
                     {}{unit})",
                    show(number),
                    show(min),
                    show(max)
                ));
            }
        }
        "enum" => {
            let v = value.trim().to_ascii_lowercase();
            let values = &setting.enumvals;
            let has = |s: &str| values.iter().any(|e| e == s);
            // The hidden spellings PG accepts besides the listed values.
            let hidden = (has("on") && has("off") && super::reloptions::parse_bool(&v).is_some())
                || (has("debug2") && v == "debug")
                || (has("notice") && v == "info");
            if !has(&v) && !hidden {
                return invalid(format!(
                    "{} (Available values: {}.)",
                    bad_value(),
                    values.join(", ")
                ));
            }
        }
        _ => {
            if name == "default_table_access_method"
                && !value.is_empty()
                && super::opclass::check_table_am(interp, value).is_err()
            {
                return invalid(format!(
                    "{} (Table access method \"{value}\" does not exist.)",
                    bad_value()
                ));
            }
        }
    }
    Ok(())
}

/// parse_int / parse_real with the parameter's unit: a number, optionally
/// followed by a memory or time unit, converted to the parameter's base
/// unit.
fn parse_with_unit(value: &str, base_unit: &str) -> Option<f64> {
    let v = value.trim();
    let split = v
        .char_indices()
        .find(|&(i, c)| {
            !(c.is_ascii_digit()
                || c == '.'
                || ((c == '-' || c == '+') && i == 0)
                || ((c == 'e' || c == 'E')
                    && v[i + c.len_utf8()..]
                        .chars()
                        .next()
                        .is_some_and(|n| n.is_ascii_digit() || n == '-' || n == '+')))
        })
        .map_or(v.len(), |(i, _)| i);
    let (num, unit) = v.split_at(split);
    let number = if let Some(hex) = num.strip_prefix("0x") {
        i64::from_str_radix(hex, 16).ok()? as f64
    } else {
        num.parse::<f64>().ok().filter(|n| n.is_finite())?
    };
    let unit = unit.trim();
    if unit.is_empty() {
        return Some(number);
    }
    let factor = |u: &str| -> Option<(bool, f64)> {
        Some(match u {
            "B" => (true, 1.0),
            "kB" => (true, 1024.0),
            "8kB" => (true, 8192.0),
            "MB" => (true, 1024.0 * 1024.0),
            "GB" => (true, 1024.0 * 1024.0 * 1024.0),
            "TB" => (true, 1024.0 * 1024.0 * 1024.0 * 1024.0),
            "us" => (false, 0.001),
            "ms" => (false, 1.0),
            "s" => (false, 1000.0),
            "min" => (false, 60_000.0),
            "h" => (false, 3_600_000.0),
            "d" => (false, 86_400_000.0),
            _ => return None,
        })
    };
    let (memory, from) = factor(unit)?;
    let (base_memory, to) = factor(base_unit)?;
    if memory != base_memory {
        return None;
    }
    Some(number * from / to)
}

/// C's `%g`.
fn format_g(v: f64) -> String {
    if v == 0.0 {
        return "0".into();
    }
    let exp = v.abs().log10().floor() as i32;
    if !(-4..6).contains(&exp) {
        let mantissa = v / 10f64.powi(exp);
        let m = format!("{mantissa:.5}");
        let m = m.trim_end_matches('0').trim_end_matches('.');
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{m}e{sign}{:02}", exp.abs())
    } else {
        let decimals = (5 - exp).max(0) as usize;
        let s = format!("{v:.decimals$}");
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_owned()
        } else {
            s
        }
    }
}
