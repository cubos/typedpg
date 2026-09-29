//! Session state set by migrations: the `search_path` GUC, and the
//! session's user identity (`SET ROLE`, `SET SESSION AUTHORIZATION`), which
//! `$user` in the search path names.
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

use typedpg_pg_query::protobuf::{
    FuncCall, SelectStmt, TransactionStmt, TransactionStmtKind, VariableSetKind, VariableSetStmt,
    a_const, node,
};

use super::DdlError;
use crate::pg_catalog::PgCatalog;

/// PG 18's predefined roles (`pg_authid` rows every cluster has). Any other
/// `pg_*` name can't be a role: CREATE ROLE reserves the prefix.
const PREDEFINED_ROLES: &[&str] = &[
    "pg_checkpoint",
    "pg_create_subscription",
    "pg_database_owner",
    "pg_execute_server_program",
    "pg_maintain",
    "pg_monitor",
    "pg_read_all_data",
    "pg_read_all_settings",
    "pg_read_all_stats",
    "pg_read_server_files",
    "pg_signal_autovacuum_worker",
    "pg_signal_backend",
    "pg_stat_scan_tables",
    "pg_use_reserved_connections",
    "pg_write_all_data",
    "pg_write_server_files",
];

/// Whether role `name` exists, as far as the catalog can tell. Roles are
/// cluster objects the analyzer doesn't see, so a name is assumed to exist
/// unless no role can bear it: `public` and `none` are reserved role names,
/// and so is every `pg_` name but the predefined roles.
pub(crate) fn role_may_exist(name: &str) -> bool {
    match name {
        "public" | "none" => false,
        _ if name.starts_with("pg_") => PREDEFINED_ROLES.contains(&name),
        _ => true,
    }
}

/// The session's user identity: the session user (`SET SESSION
/// AUTHORIZATION`) and the role on top of it (`SET ROLE`). `None` is the
/// connecting user — unknown to the analyzer — or no role.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Identity {
    session_user: Option<String>,
    role: Option<String>,
}

/// [`Identity`] at session level, and the `SET LOCAL` one for the rest of
/// the transaction.
#[derive(Clone, Debug, Default)]
pub(crate) struct SessionIdentity {
    session: Identity,
    local: Option<Identity>,
}

impl SessionIdentity {
    fn effective(&self) -> &Identity {
        self.local.as_ref().unwrap_or(&self.session)
    }

    /// `current_user`, when the migrations named it.
    pub(crate) fn current_user(&self) -> Option<&str> {
        let id = self.effective();
        id.role.as_deref().or(id.session_user.as_deref())
    }

    /// `session_user`, when the migrations named it.
    pub(crate) fn session_user(&self) -> Option<&str> {
        self.effective().session_user.as_deref()
    }

    fn update(&mut self, is_local: bool, f: impl FnOnce(&mut Identity)) {
        let mut id = self.effective().clone();
        f(&mut id);
        if is_local {
            self.local = Some(id);
        } else {
            // A session-level SET replaces a pending SET LOCAL value.
            self.local = None;
            self.session = id;
        }
    }
}

/// Textual `search_path` settings, from least to most specific.
#[derive(Clone, Debug, Default)]
pub(crate) struct SearchPathGuc {
    /// The server's setting, as the seed recorded it.
    default_setting: String,
    /// Server default: [`Self::default_setting`]'s entries.
    default: Vec<String>,
    /// Value set by a plain `SET search_path` / `set_config(..., false)`.
    session: Option<Vec<String>>,
    /// Value set by `SET LOCAL search_path` / `set_config(..., true)`.
    local: Option<Vec<String>>,
    /// Value `ALTER DATABASE ... SET search_path` gave the database's new
    /// sessions — the application's, not the migrations' own session.
    database: Option<Vec<String>>,
}

impl SearchPathGuc {
    /// The server's `search_path` setting, as text.
    pub(crate) fn with_default(setting: &str) -> Self {
        Self {
            default_setting: setting.to_owned(),
            default: split_identifier_string(setting),
            session: None,
            local: None,
            database: None,
        }
    }

    /// The setting a new session of the database starts with, as text
    /// [`Self::with_default`] reads back.
    pub(crate) fn new_session_setting(&self) -> String {
        match &self.database {
            None => self.default_setting.clone(),
            Some(names) => names
                .iter()
                .map(|n| format!("\"{}\"", n.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(", "),
        }
    }

    fn new_session(&self) -> &[String] {
        self.database.as_deref().unwrap_or(&self.default)
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
    ///
    /// Outside the migrations the setting is a new session's — the server's,
    /// or the database's (`ALTER DATABASE ... SET`): the application's
    /// queries run in sessions of their own, which a migration's `SET
    /// search_path` doesn't reach.
    pub(crate) fn refresh_search_path(&mut self) {
        let mut resolved = Vec::new();
        let setting = if self.in_migration {
            self.search_path_guc.effective()
        } else {
            self.search_path_guc.new_session()
        };
        // recomputeNamespacePath: an explicit `pg_temp` is the session's
        // temporary schema, searched where it's listed; listed first, it's
        // where new objects go even before it exists (activeTempCreationPending
        // — the analyzer creates it right away).
        let mut temp_first = false;
        for name in setting {
            // `$user` is the schema named after current_user — known only
            // in the migrations' session, and once they name the role.
            let name = match name.as_str() {
                "pg_temp" => {
                    match self.temp_namespace.filter(|_| self.in_migration) {
                        Some(temp) if !resolved.contains(&temp) => resolved.push(temp),
                        Some(_) => {}
                        None => temp_first |= resolved.is_empty() && self.in_migration,
                    }
                    continue;
                }
                "$user" => match self.session_identity.current_user() {
                    Some(user) if self.in_migration => user,
                    _ => continue,
                },
                other => other,
            };
            if let Some(oid) = self.namespace_oid(name)
                && !resolved.contains(&oid)
            {
                resolved.push(oid);
            }
        }
        if temp_first && let Ok(temp) = super::util::temp_namespace(self) {
            resolved.insert(0, temp);
        }
        self.search_path = resolved;
    }

    /// End of the current transaction: `SET LOCAL` values lapse.
    pub(crate) fn end_transaction_scope(&mut self) {
        let local_identity = self.session_identity.local.take().is_some();
        if self.search_path_guc.local.take().is_some() || local_identity {
            self.refresh_search_path();
        }
        // New enum labels are committed.
        self.uncommitted_enum_labels.clear();
        self.enums_created_in_transaction.clear();
        // ON COMMIT DROP temporary tables go.
        for table in std::mem::take(&mut self.on_commit_drop) {
            crate::ddl::drop::drop_relation_by_oid(self, table);
        }
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

/// `SET [LOCAL] ROLE` / `SET [LOCAL] SESSION AUTHORIZATION` (the `role`
/// and `session_authorization` GUCs, which pg_settings doesn't list):
/// `value` is the new name, `None` for DEFAULT / RESET. check_role /
/// check_session_authorization reject a role that doesn't exist; whether
/// the session may take it (membership, a superuser session user) depends
/// on cluster state and is not checked.
fn set_identity(
    interp: &mut PgCatalog,
    setting: &str,
    value: Option<&str>,
    is_local: bool,
) -> Result<(), DdlError> {
    let session_authorization = setting.eq_ignore_ascii_case("session_authorization");
    // SET ROLE NONE (or 'none') is RESET ROLE.
    let value = value.filter(|v| session_authorization || *v != "none");
    if let Some(name) = value
        && !role_may_exist(name)
    {
        return Err(DdlError::Parse(format!("role \"{name}\" does not exist")));
    }
    let value = value.map(str::to_owned);
    interp.session_identity.update(is_local, |id| {
        if session_authorization {
            // A new session user comes without a role.
            *id = Identity {
                session_user: value,
                role: None,
            };
        } else {
            id.role = value;
        }
    });
    interp.refresh_search_path();
    Ok(())
}

/// `SET [LOCAL] search_path ...` / `RESET search_path` / `RESET ALL`, and
/// the session identity (`SET ROLE`, `SET SESSION AUTHORIZATION`). Other
/// settings don't influence static analysis and are ignored.
pub(crate) fn variable_set(interp: &mut PgCatalog, stmt: &VariableSetStmt) -> Result<(), DdlError> {
    let kind = VariableSetKind::try_from(stmt.kind).unwrap_or(VariableSetKind::Undefined);
    if stmt.name.eq_ignore_ascii_case("role")
        || stmt.name.eq_ignore_ascii_case("session_authorization")
    {
        return match kind {
            VariableSetKind::VarSetValue => match stmt.args.as_slice() {
                [arg] => match const_arg_string(arg) {
                    Some(value) => set_identity(interp, &stmt.name, Some(&value), stmt.is_local),
                    None => Ok(()),
                },
                _ => Ok(()),
            },
            VariableSetKind::VarSetDefault | VariableSetKind::VarReset => {
                set_identity(interp, &stmt.name, None, stmt.is_local)
            }
            _ => Ok(()),
        };
    }
    // RESET ALL leaves the identity alone (GUC_NO_RESET_ALL).
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
            if let Some(names) = search_path_args(stmt) {
                interp.set_search_path(Some(names), stmt.is_local);
            }
        }
        VariableSetKind::VarSetDefault | VariableSetKind::VarReset => {
            interp.set_search_path(None, stmt.is_local);
        }
        _ => {}
    }
    Ok(())
}

/// `ALTER DATABASE name SET / RESET ...`: a setting for the database's new
/// sessions (pg_db_role_setting). The analyzer can't tell which database
/// the application connects to, so any name is taken as the migrations'
/// own — the one the application uses. Only `search_path` matters.
pub(crate) fn alter_database_set(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::protobuf::AlterDatabaseSetStmt,
) -> Result<(), DdlError> {
    let Some(set) = stmt.setstmt.as_ref() else {
        return Ok(());
    };
    let kind = VariableSetKind::try_from(set.kind).unwrap_or(VariableSetKind::Undefined);
    if kind == VariableSetKind::VarResetAll {
        interp.search_path_guc.database = None;
        interp.refresh_search_path();
        return Ok(());
    }
    // The parameter must exist and be settable, and a new value valid, as
    // for SET.
    if let Some(setting) = super::guc::check_settable(interp, &set.name)?.cloned()
        && kind == VariableSetKind::VarSetValue
        && let [arg] = set.args.as_slice()
        && let Some(value) = const_arg_string(arg)
    {
        super::guc::check_value(interp, &setting, &value)?;
    }
    if !set.name.eq_ignore_ascii_case("search_path") {
        return Ok(());
    }
    interp.search_path_guc.database = match kind {
        VariableSetKind::VarSetValue => match search_path_args(set) {
            Some(names) => Some(names),
            None => return Ok(()),
        },
        // FROM CURRENT: the session's value now.
        VariableSetKind::VarSetCurrent => Some(interp.search_path_guc.effective().to_vec()),
        _ => None,
    };
    interp.refresh_search_path();
    Ok(())
}

/// The schema names of `SET search_path = ...`, when all are constants.
fn search_path_args(stmt: &VariableSetStmt) -> Option<Vec<String>> {
    // Each argument is one schema name: the grammar hands identifiers and
    // string literals over alike, and `flatten_set_variable_args` quotes
    // every one of them (search_path is GUC_LIST_QUOTE), so `SET
    // search_path = 'a, b'` names a single schema `a, b`.
    stmt.args
        .iter()
        .map(|arg| match arg.node.as_ref() {
            Some(node::Node::AConst(c)) => match c.val.as_ref() {
                Some(a_const::Val::Sval(s)) => Some(s.sval.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
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
        if let Some((setting, value, is_local)) = constant_identity_set_config(fc) {
            set_identity(interp, &setting, Some(&value), is_local)?;
            continue;
        }
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
fn const_arg_string(arg: &typedpg_pg_query::protobuf::Node) -> Option<String> {
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
    let as_str = |n: &typedpg_pg_query::protobuf::Node| match n.node.as_ref() {
        Some(node::Node::AConst(c)) => match c.val.as_ref() {
            Some(a_const::Val::Sval(s)) => Some(s.sval.clone()),
            _ => None,
        },
        _ => None,
    };
    Some((as_str(setting)?, as_str(value)?))
}

/// `set_config('role' | 'session_authorization', '<name>', <is_local>)`
/// with constant arguments.
fn constant_identity_set_config(fc: &FuncCall) -> Option<(String, String, bool)> {
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
    let as_const = |n: &typedpg_pg_query::protobuf::Node| match n.node.as_ref() {
        Some(node::Node::AConst(c)) => c.val.clone(),
        _ => None,
    };
    let Some(a_const::Val::Sval(setting)) = as_const(setting) else {
        return None;
    };
    if !setting.sval.eq_ignore_ascii_case("role")
        && !setting.sval.eq_ignore_ascii_case("session_authorization")
    {
        return None;
    }
    let Some(a_const::Val::Sval(value)) = as_const(value) else {
        return None;
    };
    let is_local = match as_const(is_local)? {
        a_const::Val::Boolval(b) => b.boolval,
        _ => return None,
    };
    Some((setting.sval, value.sval, is_local))
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
    let as_const = |n: &typedpg_pg_query::protobuf::Node| match n.node.as_ref() {
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

/// `SET CONSTRAINTS { ALL | name [, ...] } { DEFERRED | IMMEDIATE }`
/// (AfterTriggerSetState): each name must match a constraint in its schema
/// — or, unqualified, in the first schema of the search path that has one
/// — and SET ... DEFERRED needs every match to be deferrable.
pub(crate) fn set_constraints(
    interp: &PgCatalog,
    stmt: &typedpg_pg_query::protobuf::ConstraintsSetStmt,
) -> Result<(), DdlError> {
    for constraint in &stmt.constraints {
        let Some(node::Node::RangeVar(rv)) = constraint.node.as_ref() else {
            continue;
        };
        let namespaces = if rv.schemaname.is_empty() {
            interp.schemas_for_lookup(None)
        } else {
            vec![super::util::existing_namespace(interp, &rv.schemaname)?]
        };
        let mut found = false;
        for ns in namespaces {
            for deferrable in constraint_deferrability(interp, ns, &rv.relname) {
                if !deferrable && stmt.deferred {
                    return Err(DdlError::UnsupportedDdl(format!(
                        "constraint \"{}\" is not deferrable",
                        rv.relname
                    )));
                }
                found = true;
            }
            if found {
                break;
            }
        }
        if !found {
            return Err(DdlError::TypeNotFound(format!(
                "constraint \"{}\" does not exist",
                rv.relname
            )));
        }
    }
    Ok(())
}

/// `condeferrable` of every constraint named `name` in schema `ns`
/// (`pg_constraint.connamespace`): table constraints, domain constraints
/// and constraint triggers.
fn constraint_deferrability(
    interp: &PgCatalog,
    ns: crate::oid::PgNamespaceOid,
    name: &str,
) -> Vec<bool> {
    use crate::pg_catalog::ConType;
    let mut out = Vec::new();
    for c in interp.pg_constraint.values() {
        if c.conname != name || interp.pg_class.get(&c.conrelid).map(|r| r.relnamespace) != Some(ns)
        {
            continue;
        }
        out.push(match c.contype {
            ConType::ForeignKey => interp
                .fk_details
                .get(&c.oid)
                .is_some_and(|fk| fk.deferrable()),
            // The constraint's index carries its name (indimmediate).
            ConType::PrimaryKey | ConType::Unique | ConType::Exclusion => interp
                .class_by_qname
                .get(&(ns, c.conname.clone()))
                .is_some_and(|index| interp.nonimmediate_indexes.contains(index)),
            _ => false,
        });
    }
    for (domain, constraints) in &interp.domain_constraints {
        if interp.pg_type.get(domain).map(|t| t.typnamespace) == Some(ns) {
            out.extend(constraints.iter().filter(|c| c.name == name).map(|_| false));
        }
    }
    for (relid, triggers) in &interp.triggers {
        if interp.pg_class.get(relid).map(|r| r.relnamespace) == Some(ns) {
            out.extend(
                triggers
                    .iter()
                    .filter(|t| t.name == name)
                    .filter_map(|t| t.constraint_deferrable),
            );
        }
    }
    out
}
