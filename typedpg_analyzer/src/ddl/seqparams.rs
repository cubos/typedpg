//! Sequence parameters (`pg_sequence`): CREATE / ALTER SEQUENCE options
//! are checked against each other and the sequence's data type like
//! `init_params` (sequence.c) does.

use typedpg_pg_query::ast;

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
    pub(crate) fn as_options(&self) -> Vec<Option<typedpg_pg_query::ast::Node>> {
        use typedpg_pg_query::ast::{DefElem, Float, Integer};
        let def = |name: &str, value: i128| {
            let arg = match i32::try_from(value) {
                Ok(ival) => ast::Node::Integer(Integer { ival }.into()),
                Err(_) => ast::Node::Float(
                    Float {
                        fval: value.to_string(),
                    }
                    .into(),
                ),
            };
            Some(ast::Node::DefElem(
                DefElem {
                    defname: name.to_owned(),
                    arg: Some(arg),
                    ..Default::default()
                }
                .into(),
            ))
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

/// `defGetInt64`: an integer, or a numeric literal `int8in` accepts.
pub(crate) fn numeric_arg(de: &typedpg_pg_query::ast::DefElem) -> Result<Option<i128>, DdlError> {
    let text = match de.arg.as_ref() {
        Some(ast::Node::Integer(i)) => return Ok(Some(i128::from(i.ival))),
        Some(ast::Node::Float(f)) => f.fval.as_str(),
        Some(ast::Node::String(s)) => s.sval.as_str(),
        _ => return Ok(None),
    };
    match crate::literal_input::parse_pg_integer(text) {
        Some(v) if i64::try_from(v).is_ok() => Ok(Some(v)),
        Some(_) => Err(DdlError::UnsupportedDdl(format!(
            "value \"{text}\" is out of range for type bigint"
        ))),
        None => Err(DdlError::UnsupportedDdl(
            crate::pgmsg::invalid_input_syntax_for_type("bigint", text),
        )),
    }
}

/// init_params: apply `options` to `current` (a new sequence when `None`).
pub(crate) fn init_params(
    interp: &PgCatalog,
    options: &[Option<typedpg_pg_query::ast::Node>],
    current: Option<SeqParams>,
) -> Result<SeqParams, DdlError> {
    match current {
        Some(current) => apply(interp, options, current, false, false),
        None => apply(interp, options, SeqParams::defaults(oid::INT8), true, false),
    }
}

/// init_params for the new sequence of a serial / identity column of type
/// `typ`; the identity's `SEQUENCE NAME` / `GENERATED` options aren't
/// sequence parameters. generateSerialExtraStmts prepends `AS <typ>` to the
/// options, so an `AS` of their own is a second one.
pub(crate) fn init_column_params(
    interp: &PgCatalog,
    options: &[Option<typedpg_pg_query::ast::Node>],
    typ: PgTypeOid,
) -> Result<SeqParams, DdlError> {
    apply(interp, options, SeqParams::defaults(typ), true, true)
}

fn apply(
    interp: &PgCatalog,
    options: &[Option<typedpg_pg_query::ast::Node>],
    old: SeqParams,
    is_init: bool,
    implicit_as: bool,
) -> Result<SeqParams, DdlError> {
    let mut seen: Vec<&str> = if implicit_as { vec!["as"] } else { Vec::new() };
    let mut as_type = None;
    let mut increment = None;
    let mut max: Option<Option<i128>> = None;
    let mut min: Option<Option<i128>> = None;
    let mut start = None;
    let mut restart: Option<Option<i128>> = None;
    let mut cache = None;
    for opt in options {
        let Some(ast::Node::DefElem(de)) = opt.as_ref() else {
            continue;
        };
        let name = de.defname.as_str();
        match name {
            "as" | "increment" | "maxvalue" | "minvalue" | "start" | "restart" | "cache"
            | "cycle" | "owned_by" => {
                if seen.contains(&name) {
                    return Err(DdlError::Parse("conflicting or redundant options".into()));
                }
                seen.push(name);
            }
            // The grammar takes it for identity columns only, where it is
            // filtered out before.
            "sequence_name" => {
                return Err(DdlError::Parse(
                    "invalid sequence option SEQUENCE NAME".into(),
                ));
            }
            other => {
                return Err(DdlError::UnsupportedDdl(format!(
                    "option \"{other}\" not recognized"
                )));
            }
        }
        match name {
            "as" => {
                if let Some(ast::Node::TypeName(tn)) = de.arg.as_ref() {
                    as_type = Some(super::functions::typename_type_id(interp, tn)?);
                }
            }
            "increment" => increment = numeric_arg(de)?,
            "maxvalue" => max = Some(numeric_arg(de)?),
            "minvalue" => min = Some(numeric_arg(de)?),
            "start" => start = numeric_arg(de)?,
            "restart" => restart = Some(numeric_arg(de)?),
            "cache" => cache = numeric_arg(de)?,
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
    // AS on an existing sequence resets the bounds that were the old
    // type's own (explicit ones stay, and must fit the new type).
    let reset_max = !is_init && as_type.is_some() && old.max == old_max;
    let reset_min = !is_init && as_type.is_some() && old.min == old_min;
    let typname = || super::util::format_type_for_message(interp, p.typ);

    // MAXVALUE: explicit, else NO MAXVALUE / a new sequence / a reset gets
    // the default (the type's maximum ascending, -1 descending).
    let default_max = if p.increment > 0 || reset_max {
        type_max
    } else {
        -1
    };
    match max {
        Some(Some(v)) => p.max = v,
        Some(None) => p.max = default_max,
        None if is_init || reset_max => p.max = default_max,
        None => {}
    }
    if p.max < type_min || p.max > type_max {
        return Err(DdlError::UnsupportedDdl(format!(
            "MAXVALUE ({}) is out of range for sequence data type {}",
            p.max,
            typname()
        )));
    }
    let default_min = if p.increment < 0 || reset_min {
        type_min
    } else {
        1
    };
    match min {
        Some(Some(v)) => p.min = v,
        Some(None) => p.min = default_min,
        None if is_init || reset_min => p.min = default_min,
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
