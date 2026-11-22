//! Configuration parameters set by migrations (`SET`, `RESET`,
//! `set_config`): PG knows every parameter a stock server has
//! (`pg_settings`, in the seed), refuses unknown names, parameters that
//! can't change in a session, and values of the wrong type or out of range
//! (`set_config_with_handle`, `parse_and_validate_value`).

use super::DdlError;
use crate::pg_catalog::{PgCatalog, PgSetting};

/// `map_old_guc_names` (guc.c): obsolete names find_option still resolves.
const OLD_NAMES: &[(&str, &str)] = &[
    ("sort_mem", "work_mem"),
    ("vacuum_mem", "maintenance_work_mem"),
    ("ssl_ecdh_curve", "ssl_groups"),
];

/// The parameters PostgreSQL 19 declares `GUC_NO_SHOW_ALL`
/// (`guc_parameters.dat`), which `pg_settings` — and so the seed — leaves
/// out. Known, but not modelled: any value is accepted, except that
/// `is_superuser` is `PGC_INTERNAL`.
const NOT_SHOWN: &[&str] = &[
    "default_with_oids",
    "is_superuser",
    "role",
    "seed",
    "session_authorization",
    "ssl_renegotiation_limit",
];

fn find<'a>(interp: &'a PgCatalog, name: &str) -> Option<&'a PgSetting> {
    let name = OLD_NAMES
        .iter()
        .find(|(old, _)| old.eq_ignore_ascii_case(name))
        .map_or(name, |&(_, new)| new);
    interp
        .pg_settings
        .iter()
        .find(|s| s.name.eq_ignore_ascii_case(name))
}

fn not_shown(name: &str) -> bool {
    NOT_SHOWN.iter().any(|n| n.eq_ignore_ascii_case(name))
}

/// Whether `name` is a parameter a GRANT ... ON PARAMETER may name
/// (check_GUC_name_for_parameter_acl): a known one, or a custom
/// `prefix.name` one.
pub(crate) fn exists(interp: &PgCatalog, name: &str) -> bool {
    find(interp, name).is_some() || not_shown(name) || name.contains('.')
}

/// find_option without placeholders, as `current_setting(name)` runs it
/// (GetConfigOptionByName with `missing_ok` false): an unknown name is an
/// error. A custom `prefix.name` one exists once something set it (a
/// `SET`, the server's configuration, `ALTER ROLE ... SET`), which can't be
/// known statically: accepted.
fn check_known(interp: &PgCatalog, name: &str) -> Result<(), DdlError> {
    if find(interp, name).is_some() || not_shown(name) || name.contains('.') {
        return Ok(());
    }
    Err(unrecognized(name))
}

fn unrecognized(name: &str) -> DdlError {
    DdlError::TypeNotFound(format!("unrecognized configuration parameter \"{name}\""))
}

/// The error every evaluation of a `pg_catalog` configuration function
/// over these constant arguments raises (`None` for an argument that
/// isn't a constant, `Some(None)` for a NULL one):
///
/// - `current_setting(name [, missing_ok])` (show_config_by_name[_missing_ok]):
///   an unknown name, unless `missing_ok` is true (the result is then NULL);
/// - `set_config(name, value, is_local)` (set_config_by_name): a NULL name,
///   then what `SET name TO value` would refuse (set_config_option) — a
///   NULL value is a reset, which only needs a settable parameter.
///
/// A NULL argument of the strict `current_setting` makes the call NULL
/// before it runs: the caller doesn't get here.
pub(crate) fn constant_call_error(
    interp: &PgCatalog,
    proname: &str,
    args: &[Option<Option<ConstArg<'_>>>],
) -> Result<(), DdlError> {
    match (proname, args) {
        ("current_setting", [Some(Some(ConstArg::Str(name)))]) => check_known(interp, name),
        (
            "current_setting",
            [
                Some(Some(ConstArg::Str(name))),
                Some(Some(ConstArg::Bool(false))),
            ],
        ) => check_known(interp, name),
        ("set_config", [Some(None), _, _]) => invalid("SET requires parameter name".to_string()),
        ("set_config", [Some(Some(ConstArg::Str(name))), value, _]) => {
            let Some(setting) = check_settable(interp, name)?.cloned() else {
                return Ok(());
            };
            match value {
                Some(Some(ConstArg::Str(value))) => check_value(interp, &setting, value),
                _ => Ok(()),
            }
        }
        _ => Ok(()),
    }
}

/// A constant argument of a configuration function (see
/// [`constant_call_error`]).
#[derive(Debug, Clone, Copy)]
pub(crate) enum ConstArg<'a> {
    Str(&'a str),
    Bool(bool),
}

impl<'a> ConstArg<'a> {
    /// `None` when `node` isn't a constant, `Some(None)` for `NULL`.
    pub(crate) fn of(node: Option<&'a typedpg_pg_query::ast::Node>) -> Option<Option<Self>> {
        use typedpg_pg_query::ast::{AConstValue, Node};
        let Some(Node::AConst(c)) = node else {
            return None;
        };
        if c.isnull {
            return Some(None);
        }
        match c.val.as_ref()? {
            AConstValue::String(s) => Some(Some(ConstArg::Str(&s.sval))),
            AConstValue::Boolean(b) => Some(Some(ConstArg::Bool(b.boolval))),
            _ => None,
        }
    }
}

fn invalid(err: String) -> Result<(), DdlError> {
    Err(DdlError::UnsupportedDdl(err))
}

/// The parameters PostgreSQL 19 declares `GUC_LIST_INPUT`
/// (`guc_parameters.dat`).
const LIST_INPUT: &[&str] = &[
    "createrole_self_grant",
    "DateStyle",
    "debug_io_direct",
    "listen_addresses",
    "local_preload_libraries",
    "log_connections",
    "log_destination",
    "log_min_messages",
    "oauth_validator_libraries",
    "output_plugin_libraries",
    "restrict_nonsystem_relation_kind",
    "search_path",
    "session_preload_libraries",
    "shared_preload_libraries",
    "synchronized_standby_slots",
    "synchronous_standby_names",
    "temp_tablespaces",
    "unix_socket_directories",
    "wal_consistency_checking",
];

/// flatten_set_variable_args (PostgreSQL 19): `SET name TO NULL` sets a list
/// parameter to the empty list and is an error for any other parameter —
/// before the name is even looked up. Returns whether `args` is that NULL.
pub(crate) fn null_arg(
    name: &str,
    args: &[Option<typedpg_pg_query::ast::Node>],
) -> Result<bool, DdlError> {
    let [Some(typedpg_pg_query::ast::Node::AConst(c))] = args else {
        return Ok(false);
    };
    if !c.isnull {
        return Ok(false);
    }
    if !LIST_INPUT.iter().any(|l| l.eq_ignore_ascii_case(name)) {
        invalid(format!("NULL is an invalid value for {name}"))?;
    }
    Ok(true)
}

/// The parameter must exist (or be a custom `prefix.name` one) and be
/// changeable in a session. Returns it when known.
pub(crate) fn check_settable<'a>(
    interp: &'a PgCatalog,
    name: &str,
) -> Result<Option<&'a PgSetting>, DdlError> {
    let Some(setting) = find(interp, name) else {
        if name.eq_ignore_ascii_case("is_superuser") {
            return Err(DdlError::UnsupportedDdl(
                "parameter \"is_superuser\" cannot be changed".to_string(),
            ));
        }
        if name.contains('.') || not_shown(name) {
            return Ok(None);
        }
        return Err(unrecognized(name));
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
            let Some(on) = super::reloptions::parse_bool(value) else {
                return invalid(format!("parameter \"{name}\" requires a Boolean value"));
            };
            // check_standard_conforming_strings: PostgreSQL 19 keeps it on.
            if !on && name == "standard_conforming_strings" {
                return invalid("non-standard string literals are not supported".into());
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
pub(crate) fn parse_with_unit(value: &str, base_unit: &str) -> Option<f64> {
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

/// A memory value in kB (`parse_int` with `GUC_UNIT_KB`): a bare number is
/// kB, a unit converts.
pub(crate) fn parse_memory_kb(value: &str) -> Option<f64> {
    parse_with_unit(value, "kB")
}
