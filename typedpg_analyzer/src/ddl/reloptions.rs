//! Storage parameters (`WITH (...)`, `SET (...)`, `RESET (...)`) of
//! tables, materialized views, indexes and views: PG parses them against
//! the relation kind's option table (reloptions.c) and rejects unknown
//! names, malformed values and out-of-range numbers.

use typedpg_pg_query::protobuf::node;

use super::DdlError;

/// Which option table applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RelOptKind {
    /// Tables and materialized views (RELOPT_KIND_HEAP, `toast.` too).
    Heap,
    View,
    Partitioned,
    /// An index of the named access method.
    Index(IndexAm),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IndexAm {
    Btree,
    Hash,
    Gist,
    Gin,
    Spgist,
    Brin,
}

impl IndexAm {
    pub(crate) fn from_name(am: &str) -> Option<IndexAm> {
        Some(match am {
            "btree" => IndexAm::Btree,
            "hash" => IndexAm::Hash,
            "gist" => IndexAm::Gist,
            "gin" => IndexAm::Gin,
            "spgist" => IndexAm::Spgist,
            "brin" => IndexAm::Brin,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug)]
enum OptType {
    Bool,
    Int(i64, i64),
    Real(f64, f64),
    Enum(&'static [&'static str], &'static str),
}

const INT_MAX: i64 = i32::MAX as i64;
const ON_OFF_AUTO: &[&str] = &["on", "off", "auto"];
const ON_OFF_AUTO_DETAIL: &str = "Valid values are \"on\", \"off\", and \"auto\".";

/// The autovacuum / vacuum options shared by heap and TOAST relations.
fn heap_and_toast(name: &str) -> Option<OptType> {
    Some(match name {
        "autovacuum_enabled" | "vacuum_truncate" => OptType::Bool,
        "autovacuum_vacuum_threshold" => OptType::Int(0, INT_MAX),
        "autovacuum_vacuum_max_threshold" | "autovacuum_vacuum_insert_threshold" => {
            OptType::Int(-1, INT_MAX)
        }
        "autovacuum_vacuum_cost_limit" => OptType::Int(1, 10000),
        "autovacuum_freeze_min_age" | "autovacuum_multixact_freeze_min_age" => {
            OptType::Int(0, 1_000_000_000)
        }
        "autovacuum_freeze_max_age" => OptType::Int(100_000, 2_000_000_000),
        "autovacuum_multixact_freeze_max_age" => OptType::Int(10_000, 2_000_000_000),
        "autovacuum_freeze_table_age" | "autovacuum_multixact_freeze_table_age" => {
            OptType::Int(0, 2_000_000_000)
        }
        "log_autovacuum_min_duration" => OptType::Int(-1, INT_MAX),
        "autovacuum_vacuum_cost_delay" => OptType::Real(0.0, 100.0),
        "autovacuum_vacuum_scale_factor" | "autovacuum_vacuum_insert_scale_factor" => {
            OptType::Real(0.0, 100.0)
        }
        "vacuum_index_cleanup" => OptType::Enum(ON_OFF_AUTO, ON_OFF_AUTO_DETAIL),
        _ => return None,
    })
}

fn option_type(kind: RelOptKind, namespace: Option<&str>, name: &str) -> Option<OptType> {
    match (kind, namespace) {
        (RelOptKind::Heap, Some("toast")) => heap_and_toast(name),
        (RelOptKind::Heap, None) => heap_and_toast(name).or(match name {
            "fillfactor" => Some(OptType::Int(10, 100)),
            "user_catalog_table" => Some(OptType::Bool),
            "autovacuum_analyze_threshold" => Some(OptType::Int(0, INT_MAX)),
            "autovacuum_analyze_scale_factor" => Some(OptType::Real(0.0, 100.0)),
            "toast_tuple_target" => Some(OptType::Int(128, 8160)),
            "parallel_workers" => Some(OptType::Int(0, 1024)),
            "vacuum_max_eager_freeze_failure_rate" => Some(OptType::Real(0.0, 1.0)),
            _ => None,
        }),
        (RelOptKind::View, None) => Some(match name {
            "security_barrier" | "security_invoker" => OptType::Bool,
            "check_option" => OptType::Enum(
                &["local", "cascaded"],
                "Valid values are \"local\" and \"cascaded\".",
            ),
            _ => return None,
        }),
        (RelOptKind::Index(am), None) => Some(match (am, name) {
            (IndexAm::Btree | IndexAm::Hash | IndexAm::Gist | IndexAm::Spgist, "fillfactor") => {
                OptType::Int(10, 100)
            }
            (IndexAm::Btree, "deduplicate_items") => OptType::Bool,
            (IndexAm::Gist, "buffering") => OptType::Enum(ON_OFF_AUTO, ON_OFF_AUTO_DETAIL),
            (IndexAm::Gin, "fastupdate") => OptType::Bool,
            (IndexAm::Gin, "gin_pending_list_limit") => OptType::Int(64, INT_MAX / 1024),
            (IndexAm::Brin, "pages_per_range") => OptType::Int(1, 131_072),
            (IndexAm::Brin, "autosummarize") => OptType::Bool,
            _ => return None,
        }),
        _ => None,
    }
}

/// defGetString of an option's value; a bare name means "true".
fn value_string(de: &typedpg_pg_query::protobuf::DefElem) -> Option<String> {
    Some(match de.arg.as_deref().and_then(|a| a.node.as_ref()) {
        None => "true".to_owned(),
        Some(node::Node::Integer(i)) => i.ival.to_string(),
        Some(node::Node::Float(f)) => f.fval.clone(),
        Some(node::Node::String(s)) => s.sval.clone(),
        Some(node::Node::Boolean(b)) => b.boolval.to_string(),
        Some(node::Node::TypeName(tn)) => super::util::type_name_to_string(tn),
        _ => return None,
    })
}

/// parse_bool: `true`/`false`/`yes`/`no`/`on`/`off`/`1`/`0` and their
/// unambiguous prefixes.
pub(crate) fn parse_bool(s: &str) -> Option<bool> {
    let v = s.trim().to_ascii_lowercase();
    if v.is_empty() {
        return None;
    }
    let prefix = |full: &str| full.starts_with(v.as_str());
    match v.as_str() {
        "1" => Some(true),
        "0" => Some(false),
        _ if prefix("true") || prefix("yes") => Some(true),
        _ if prefix("false") || prefix("no") => Some(false),
        _ if v.len() >= 2 && prefix("on") => Some(true),
        _ if v.len() >= 2 && prefix("off") => Some(false),
        _ => None,
    }
}

/// parse_int: a decimal / hex / octal integer, or a decimal fraction
/// rounded.
fn parse_int(s: &str) -> Option<i64> {
    let v = s.trim();
    let (neg, body) = match v.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, v.strip_prefix('+').unwrap_or(v)),
    };
    let parsed = if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16).ok()
    } else if body.len() > 1 && body.starts_with('0') && body.bytes().all(|b| b.is_ascii_digit()) {
        i64::from_str_radix(&body[1..], 8).ok()
    } else if let Ok(i) = body.parse::<i64>() {
        Some(i)
    } else {
        body.parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .map(|f| f.round() as i64)
    }?;
    Some(if neg { -parsed } else { parsed })
}

fn check_value(name: &str, typ: OptType, value: &str) -> Result<(), DdlError> {
    let err = |msg: String| Err(DdlError::UnsupportedDdl(msg));
    match typ {
        OptType::Bool => {
            if parse_bool(value).is_none() {
                return err(format!(
                    "invalid value for boolean option \"{name}\": {value}"
                ));
            }
        }
        OptType::Int(min, max) => {
            let Some(v) = parse_int(value).filter(|v| i32::try_from(*v).is_ok()) else {
                return err(format!(
                    "invalid value for integer option \"{name}\": {value}"
                ));
            };
            if v < min || v > max {
                return err(format!(
                    "value {value} out of bounds for option \"{name}\" (Valid values are \
                     between \"{min}\" and \"{max}\".)"
                ));
            }
        }
        OptType::Real(min, max) => {
            let Some(v) = value.trim().parse::<f64>().ok().filter(|v| v.is_finite()) else {
                return err(format!(
                    "invalid value for floating point option \"{name}\": {value}"
                ));
            };
            if v < min || v > max {
                return err(format!(
                    "value {value} out of bounds for option \"{name}\" (Valid values are \
                     between \"{min:.6}\" and \"{max:.6}\".)"
                ));
            }
        }
        OptType::Enum(values, detail) => {
            let ok = values.iter().any(|v| v.eq_ignore_ascii_case(value))
                || (values == ON_OFF_AUTO && parse_bool(value).is_some());
            if !ok {
                return err(format!(
                    "invalid value for enum option \"{name}\": {value} ({detail})"
                ));
            }
        }
    }
    Ok(())
}

/// transformRelOptions + the kind's `*_reloptions`: validate a `WITH` /
/// `SET` list, or the names of a `RESET` list.
pub(crate) fn check_reloptions(
    options: &[typedpg_pg_query::protobuf::Node],
    kind: RelOptKind,
    reset: bool,
    accept_oids_off: bool,
) -> Result<(), DdlError> {
    let mut seen: Vec<(String, String)> = Vec::new();
    let mut any = false;
    for opt in options {
        let Some(node::Node::DefElem(de)) = opt.node.as_ref() else {
            continue;
        };
        if reset {
            if de.arg.is_some() {
                return Err(DdlError::Parse(
                    "RESET must not include values for parameters".into(),
                ));
            }
            continue;
        }
        if accept_oids_off && de.defnamespace.is_empty() && de.defname == "oids" {
            if value_string(de).as_deref().and_then(parse_bool) == Some(true) {
                return Err(DdlError::UnsupportedDdl(
                    "tables declared WITH OIDS are not supported".into(),
                ));
            }
            continue;
        }
        let namespace = (!de.defnamespace.is_empty()).then_some(de.defnamespace.as_str());
        if let Some(ns) = namespace
            && !(kind == RelOptKind::Heap && ns == "toast")
        {
            return Err(DdlError::UnsupportedDdl(format!(
                "unrecognized parameter namespace \"{ns}\""
            )));
        }
        any = true;
        if kind == RelOptKind::Partitioned {
            continue;
        }
        let key = (de.defnamespace.clone(), de.defname.clone());
        let Some(typ) = option_type(kind, namespace, &de.defname) else {
            return Err(DdlError::UnsupportedDdl(format!(
                "unrecognized parameter \"{}\"",
                de.defname
            )));
        };
        if seen.contains(&key) {
            return Err(DdlError::UnsupportedDdl(format!(
                "parameter \"{}\" specified more than once",
                de.defname
            )));
        }
        seen.push(key);
        if let Some(value) = value_string(de) {
            check_value(&de.defname, typ, &value)?;
        }
    }
    if kind == RelOptKind::Partitioned && any {
        return Err(DdlError::UnsupportedDdl(
            "cannot specify storage parameters for a partitioned table (Specify storage \
             parameters for its leaf partitions instead.)"
                .into(),
        ));
    }
    Ok(())
}
