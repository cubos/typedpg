//! Sequence parameters (`pg_sequence`): CREATE / ALTER SEQUENCE options
//! are checked against each other and the sequence's data type like
//! `init_params` (sequence.c) does.

use typedpg_pg_query::protobuf::node;

use super::DdlError;
use crate::oid::PgTypeOid;
use crate::pg_catalog::{PgCatalog, oid};

/// A sequence's `pg_sequence` row.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SeqParams {
    pub(crate) typ: PgTypeOid,
    pub(crate) increment: i128,
    pub(crate) min: i128,
    pub(crate) max: i128,
    pub(crate) start: i128,
    pub(crate) cache: i128,
}

fn type_range(typ: PgTypeOid) -> (i128, i128) {
    if typ == oid::INT2 {
        (i128::from(i16::MIN), i128::from(i16::MAX))
    } else if typ == oid::INT4 {
        (i128::from(i32::MIN), i128::from(i32::MAX))
    } else {
        (i128::from(i64::MIN), i128::from(i64::MAX))
    }
}

impl SeqParams {
    /// The defaults of a new sequence of type `typ`.
    pub(crate) fn defaults(typ: PgTypeOid) -> SeqParams {
        let (_, type_max) = type_range(typ);
        SeqParams {
            typ,
            increment: 1,
            min: 1,
            max: type_max,
            start: 1,
            cache: 1,
        }
    }
}

impl SeqParams {
    /// sequence_options (sequence.c): these parameters as the option list
    /// that recreates them.
    pub(crate) fn as_options(&self) -> Vec<typedpg_pg_query::protobuf::Node> {
        use typedpg_pg_query::protobuf::{DefElem, Float, Integer, Node};
        let def = |name: &str, value: i128| {
            let arg = match i32::try_from(value) {
                Ok(ival) => node::Node::Integer(Integer { ival }),
                Err(_) => node::Node::Float(Float {
                    fval: value.to_string(),
                }),
            };
            Node {
                node: Some(node::Node::DefElem(Box::new(DefElem {
                    defname: name.to_owned(),
                    arg: Some(Box::new(Node { node: Some(arg) })),
                    ..Default::default()
                }))),
            }
        };
        vec![
            def("cache", self.cache),
            def("increment", self.increment),
            def("maxvalue", self.max),
            def("minvalue", self.min),
            def("start", self.start),
        ]
    }
}

fn numeric_arg(de: &typedpg_pg_query::protobuf::DefElem) -> Option<i128> {
    match de.arg.as_deref().and_then(|a| a.node.as_ref())? {
        node::Node::Integer(i) => Some(i128::from(i.ival)),
        node::Node::Float(f) => f.fval.parse().ok(),
        node::Node::String(s) => s.sval.parse().ok(),
        _ => None,
    }
}

/// init_params: apply `options` to `current` (a new sequence when `None`).
pub(crate) fn init_params(
    interp: &PgCatalog,
    options: &[typedpg_pg_query::protobuf::Node],
    current: Option<SeqParams>,
) -> Result<SeqParams, DdlError> {
    match current {
        Some(current) => apply(interp, options, current, false),
        None => apply(interp, options, SeqParams::defaults(oid::INT8), true),
    }
}

/// init_params for the new sequence of a serial / identity column of type
/// `typ`; the identity's `SEQUENCE NAME` / `GENERATED` options aren't
/// sequence parameters.
pub(crate) fn init_column_params(
    interp: &PgCatalog,
    options: &[typedpg_pg_query::protobuf::Node],
    typ: PgTypeOid,
) -> Result<SeqParams, DdlError> {
    apply(interp, options, SeqParams::defaults(typ), true)
}

fn apply(
    interp: &PgCatalog,
    options: &[typedpg_pg_query::protobuf::Node],
    old: SeqParams,
    is_init: bool,
) -> Result<SeqParams, DdlError> {
    let mut seen: Vec<&str> = Vec::new();
    let mut as_type = None;
    let mut increment = None;
    let mut max: Option<Option<i128>> = None;
    let mut min: Option<Option<i128>> = None;
    let mut start = None;
    let mut restart: Option<Option<i128>> = None;
    let mut cache = None;
    for opt in options {
        let Some(node::Node::DefElem(de)) = opt.node.as_ref() else {
            continue;
        };
        let name = de.defname.as_str();
        if matches!(
            name,
            "as" | "increment" | "maxvalue" | "minvalue" | "start" | "restart" | "cache" | "cycle"
        ) {
            if seen.contains(&name) {
                return Err(DdlError::Parse("conflicting or redundant options".into()));
            }
            seen.push(name);
        }
        match name {
            "as" => {
                if let Some(node::Node::TypeName(tn)) =
                    de.arg.as_deref().and_then(|a| a.node.as_ref())
                {
                    as_type = Some(super::util::lookup_type_name(tn, interp)?);
                }
            }
            "increment" => increment = numeric_arg(de),
            "maxvalue" => max = Some(numeric_arg(de)),
            "minvalue" => min = Some(numeric_arg(de)),
            "start" => start = numeric_arg(de),
            "restart" => restart = Some(numeric_arg(de)),
            "cache" => cache = numeric_arg(de),
            _ => {}
        }
    }

    let mut p = old;
    if let Some(typ) = as_type {
        if typ != oid::INT2 && typ != oid::INT4 && typ != oid::INT8 {
            return Err(DdlError::UnsupportedDdl(
                "sequence type must be smallint, integer, or bigint".into(),
            ));
        }
        p.typ = typ;
    }
    if let Some(inc) = increment {
        if inc == 0 {
            return Err(DdlError::UnsupportedDdl(
                "INCREMENT must not be zero".into(),
            ));
        }
        p.increment = inc;
    }
    let (type_min, type_max) = type_range(p.typ);
    let (old_min, old_max) = type_range(old.typ);
    let type_changed = as_type.is_some_and(|t| t != old.typ);
    let typname = || super::util::format_type_for_message(interp, p.typ);

    // MAXVALUE: explicit, NO MAXVALUE / new sequence => default, or reset
    // when the type changed and it was the old type's bound.
    match max {
        Some(Some(v)) => p.max = v,
        Some(None) => p.max = if p.increment > 0 { type_max } else { -1 },
        None if is_init => p.max = if p.increment > 0 { type_max } else { -1 },
        None if type_changed && (old.max == old_max || old.max > type_max) => {
            p.max = if p.increment > 0 { type_max } else { -1 };
        }
        None => {}
    }
    if p.max < type_min || p.max > type_max {
        return Err(DdlError::UnsupportedDdl(format!(
            "MAXVALUE ({}) is out of range for sequence data type {}",
            p.max,
            typname()
        )));
    }
    match min {
        Some(Some(v)) => p.min = v,
        Some(None) => p.min = if p.increment > 0 { 1 } else { type_min },
        None if is_init => p.min = if p.increment > 0 { 1 } else { type_min },
        None if type_changed && (old.min == old_min || old.min < type_min) => {
            p.min = if p.increment > 0 { 1 } else { type_min };
        }
        None => {}
    }
    if p.min < type_min || p.min > type_max {
        return Err(DdlError::UnsupportedDdl(format!(
            "MINVALUE ({}) is out of range for sequence data type {}",
            p.min,
            typname()
        )));
    }
    if p.min >= p.max {
        return Err(DdlError::UnsupportedDdl(format!(
            "MINVALUE ({}) must be less than MAXVALUE ({})",
            p.min, p.max
        )));
    }
    match start {
        Some(v) => p.start = v,
        None if is_init => p.start = if p.increment > 0 { p.min } else { p.max },
        None => {}
    }
    if p.start < p.min {
        return Err(DdlError::UnsupportedDdl(format!(
            "START value ({}) cannot be less than MINVALUE ({})",
            p.start, p.min
        )));
    }
    if p.start > p.max {
        return Err(DdlError::UnsupportedDdl(format!(
            "START value ({}) cannot be greater than MAXVALUE ({})",
            p.start, p.max
        )));
    }
    if let Some(value) = restart {
        let value = value.unwrap_or(p.start);
        if value < p.min {
            return Err(DdlError::UnsupportedDdl(format!(
                "RESTART value ({value}) cannot be less than MINVALUE ({})",
                p.min
            )));
        }
        if value > p.max {
            return Err(DdlError::UnsupportedDdl(format!(
                "RESTART value ({value}) cannot be greater than MAXVALUE ({})",
                p.max
            )));
        }
    }
    if let Some(c) = cache {
        if c <= 0 {
            return Err(DdlError::UnsupportedDdl(format!(
                "CACHE ({c}) must be greater than zero"
            )));
        }
        p.cache = c;
    }
    Ok(p)
}
