//! Session state set by migrations: the `search_path` GUC.
//!
//! PG keeps `search_path` as a list of schema *names* and resolves it
//! lazily (`recomputeNamespacePath` in `namespace.c`): entries naming a
//! schema that does not exist are skipped, and start to count as soon as the
//! schema is created. The catalog mirrors that by storing the textual
//! setting in [`SearchPathGuc`] and re-deriving the resolved
//! [`PgCatalog::search_path`] OID list whenever the setting or the set of
//! schemas changes.
//!
//! Scoping follows a migration run by the runner — one connection, one
//! transaction per migration (= one `apply_sql` call): `SET` lasts for the
//! session, `SET LOCAL` until the end of the transaction (`COMMIT` /
//! `ROLLBACK` or the end of the `apply_sql` call).

use pg_query::protobuf::{
    FuncCall, SelectStmt, TransactionStmt, TransactionStmtKind, VariableSetKind, VariableSetStmt,
    a_const, node,
};

use super::DdlError;
use crate::pg_catalog::PgCatalog;

/// Textual `search_path` settings, from least to most specific.
#[derive(Clone, Debug, Default)]
pub(crate) struct SearchPathGuc {
    /// Server default (the seed's resolved path).
    default: Vec<String>,
    /// Value set by a plain `SET search_path` / `set_config(..., false)`.
    session: Option<Vec<String>>,
    /// Value set by `SET LOCAL search_path` / `set_config(..., true)`.
    local: Option<Vec<String>>,
}

impl SearchPathGuc {
    pub(crate) fn with_default(default: Vec<String>) -> Self {
        Self {
            default,
            session: None,
            local: None,
        }
    }

    fn effective(&self) -> &[String] {
        self.local
            .as_deref()
            .or(self.session.as_deref())
            .unwrap_or(&self.default)
    }
}

impl PgCatalog {
    /// Re-derive [`PgCatalog::search_path`] from the textual setting: every
    /// entry naming an existing schema, in order, without duplicates.
    /// `$user` and `pg_temp` never name a schema the analyzer models.
    pub(crate) fn refresh_search_path(&mut self) {
        let mut resolved = Vec::new();
        for name in self.search_path_guc.effective() {
            if name == "$user" || name == "pg_temp" {
                continue;
            }
            if let Some(oid) = self.namespace_oid(name)
                && !resolved.contains(&oid)
            {
                resolved.push(oid);
            }
        }
        self.search_path = resolved;
    }

    /// End of the current transaction: `SET LOCAL` values lapse.
    pub(crate) fn end_transaction_scope(&mut self) {
        if self.search_path_guc.local.take().is_some() {
            self.refresh_search_path();
        }
        // New enum labels are committed.
        self.uncommitted_enum_labels.clear();
        self.enums_created_in_transaction.clear();
    }

    /// Extension-script / CREATE SCHEMA element scope: `schema` goes in front of the
    /// current path until [`Self::restore_search_path`] puts back the
    /// returned settings.
    pub(crate) fn push_search_path_front(&mut self, schema: &str) -> SearchPathGuc {
        let saved = self.search_path_guc.clone();
        let mut names = vec![schema.to_owned()];
        names.extend(saved.effective().iter().filter(|n| *n != schema).cloned());
        self.search_path_guc.local = Some(names);
        self.refresh_search_path();
        saved
    }

    pub(crate) fn restore_search_path(&mut self, saved: SearchPathGuc) {
        self.search_path_guc = saved;
        self.refresh_search_path();
    }

    fn set_search_path(&mut self, value: Option<Vec<String>>, is_local: bool) {
        if is_local {
            self.search_path_guc.local = value;
        } else {
            // A session-level SET also replaces any pending SET LOCAL value
            // for the rest of the transaction (PG's GUC stack does the same).
            self.search_path_guc.local = None;
            self.search_path_guc.session = value;
        }
        self.refresh_search_path();
    }
}

/// `SET [LOCAL] search_path ...` / `RESET search_path` / `RESET ALL`.
/// Other settings don't influence static analysis and are ignored.
pub(crate) fn variable_set(interp: &mut PgCatalog, stmt: &VariableSetStmt) -> Result<(), DdlError> {
    let kind = VariableSetKind::try_from(stmt.kind).unwrap_or(VariableSetKind::Undefined);
    if kind == VariableSetKind::VarResetAll {
        interp.set_search_path(None, false);
        interp.check_function_bodies = true;
        return Ok(());
    }
    // The parameter must exist and be settable, and a new value valid.
    if matches!(
        kind,
        VariableSetKind::VarSetValue
            | VariableSetKind::VarSetDefault
            | VariableSetKind::VarSetCurrent
            | VariableSetKind::VarReset
    ) && let Some(setting) = super::guc::check_settable(interp, &stmt.name)?.cloned()
        && kind == VariableSetKind::VarSetValue
        && let [arg] = stmt.args.as_slice()
        && let Some(value) = const_arg_string(arg)
    {
        super::guc::check_value(interp, &setting, &value)?;
    }
    if stmt.name.eq_ignore_ascii_case("check_function_bodies") {
        interp.check_function_bodies = match kind {
            VariableSetKind::VarSetValue => stmt.args.first().is_none_or(|arg| {
                !matches!(arg.node.as_ref(), Some(node::Node::AConst(c)) if match c.val.as_ref() {
                    Some(a_const::Val::Sval(s)) => matches!(
                        s.sval.to_ascii_lowercase().as_str(),
                        "off" | "false" | "no" | "0" | "f" | "n"
                    ),
                    Some(a_const::Val::Ival(i)) => i.ival == 0,
                    Some(a_const::Val::Boolval(b)) => !b.boolval,
                    _ => false,
                })
            }),
            _ => true,
        };
        return Ok(());
    }
    if !stmt.name.eq_ignore_ascii_case("search_path") {
        return Ok(());
    }
    match kind {
        VariableSetKind::VarSetValue => {
            // Each argument is one schema name: the grammar hands identifiers
            // and string literals over alike, and `flatten_set_variable_args`
            // quotes every one of them (search_path is GUC_LIST_QUOTE), so
            // `SET search_path = 'a, b'` names a single schema `a, b`.
            let mut names = Vec::new();
            for arg in &stmt.args {
                match arg.node.as_ref() {
                    Some(node::Node::AConst(c)) => match c.val.as_ref() {
                        Some(a_const::Val::Sval(s)) => names.push(s.sval.clone()),
                        _ => return Ok(()),
                    },
                    _ => return Ok(()),
                }
            }
            interp.set_search_path(Some(names), stmt.is_local);
        }
        VariableSetKind::VarSetDefault | VariableSetKind::VarReset => {
            interp.set_search_path(None, stmt.is_local);
        }
        _ => {}
    }
    Ok(())
}

/// `COMMIT` / `ROLLBACK` (and their prepared / savepoint-less variants) end
/// the transaction scope of `SET LOCAL`.
pub(crate) fn transaction(interp: &mut PgCatalog, stmt: &TransactionStmt) -> Result<(), DdlError> {
    let kind = TransactionStmtKind::try_from(stmt.kind).unwrap_or(TransactionStmtKind::Undefined);
    if matches!(
        kind,
        TransactionStmtKind::TransStmtCommit
            | TransactionStmtKind::TransStmtRollback
            | TransactionStmtKind::TransStmtPrepare
    ) {
        interp.end_transaction_scope();
    }
    Ok(())
}

/// A top-level `SELECT` in a migration is not analyzed, but
/// `SELECT [pg_catalog.]set_config('search_path', '<list>', <is_local>)` —
/// the form `pg_dump` emits — changes the session's search path. Apply it
/// when all three arguments are constants (anything else can't be known
/// statically).
pub(crate) fn select_side_effects(
    interp: &mut PgCatalog,
    stmt: &SelectStmt,
) -> Result<(), DdlError> {
    if !stmt.from_clause.is_empty() || stmt.where_clause.is_some() {
        return Ok(());
    }
    for target in &stmt.target_list {
        let Some(node::Node::ResTarget(rt)) = target.node.as_ref() else {
            continue;
        };
        let Some(node::Node::FuncCall(fc)) = rt.val.as_deref().and_then(|v| v.node.as_ref()) else {
            continue;
        };
        if let Some((setting, value)) = constant_set_config(fc)
            && let Some(known) = super::guc::check_settable(interp, &setting)?.cloned()
        {
            super::guc::check_value(interp, &known, &value)?;
        }
        if let Some((value, is_local)) = constant_search_path_set_config(fc) {
            interp.set_search_path(Some(split_identifier_string(&value)), is_local);
        }
    }
    Ok(())
}

/// A constant `SET` argument as the string GUC code parses.
fn const_arg_string(arg: &pg_query::protobuf::Node) -> Option<String> {
    match arg.node.as_ref() {
        Some(node::Node::AConst(c)) => match c.val.as_ref()? {
            a_const::Val::Ival(i) => Some(i.ival.to_string()),
            a_const::Val::Fval(f) => Some(f.fval.clone()),
            a_const::Val::Sval(s) => Some(s.sval.clone()),
            a_const::Val::Boolval(b) => Some(b.boolval.to_string()),
            a_const::Val::Bsval(_) => None,
        },
        _ => None,
    }
}

/// `set_config('<name>', '<value>', ...)` with constant name and value.
fn constant_set_config(fc: &FuncCall) -> Option<(String, String)> {
    let name: Vec<&str> = fc
        .funcname
        .iter()
        .filter_map(super::util::node_string)
        .collect();
    if !matches!(
        name.as_slice(),
        ["set_config"] | ["pg_catalog", "set_config"]
    ) {
        return None;
    }
    let [setting, value, _] = fc.args.as_slice() else {
        return None;
    };
    let as_str = |n: &pg_query::protobuf::Node| match n.node.as_ref() {
        Some(node::Node::AConst(c)) => match c.val.as_ref() {
            Some(a_const::Val::Sval(s)) => Some(s.sval.clone()),
            _ => None,
        },
        _ => None,
    };
    Some((as_str(setting)?, as_str(value)?))
}

fn constant_search_path_set_config(fc: &FuncCall) -> Option<(String, bool)> {
    let name: Vec<&str> = fc
        .funcname
        .iter()
        .filter_map(super::util::node_string)
        .collect();
    if !matches!(
        name.as_slice(),
        ["set_config"] | ["pg_catalog", "set_config"]
    ) {
        return None;
    }
    let [setting, value, is_local] = fc.args.as_slice() else {
        return None;
    };
    let as_const = |n: &pg_query::protobuf::Node| match n.node.as_ref() {
        Some(node::Node::AConst(c)) => c.val.clone(),
        _ => None,
    };
    let Some(a_const::Val::Sval(setting)) = as_const(setting) else {
        return None;
    };
    if !setting.sval.eq_ignore_ascii_case("search_path") {
        return None;
    }
    let Some(a_const::Val::Sval(value)) = as_const(value) else {
        return None;
    };
    let is_local = match as_const(is_local)? {
        a_const::Val::Boolval(b) => b.boolval,
        _ => return None,
    };
    Some((value.sval, is_local))
}

/// PG's `SplitIdentifierString(value, ',')` (`varlena.c`): split a GUC list
/// on commas, trimming whitespace; `"..."` entries keep their case (with
/// `""` as an escaped quote), bare ones are downcased.
fn split_identifier_string(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = value.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.peek().is_none() {
            break;
        }
        let mut name = String::new();
        if chars.next_if_eq(&'"').is_some() {
            while let Some(c) = chars.next() {
                if c == '"' {
                    if chars.next_if_eq(&'"').is_some() {
                        name.push('"');
                    } else {
                        break;
                    }
                } else {
                    name.push(c);
                }
            }
        } else {
            while let Some(c) = chars.next_if(|&c| c != ',' && !c.is_whitespace()) {
                name.push(c.to_ascii_lowercase());
            }
        }
        out.push(name);
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.next_if_eq(&',').is_none() {
            break;
        }
    }
    out
}
