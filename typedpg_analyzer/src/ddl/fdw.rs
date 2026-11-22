//! Foreign-data wrappers, foreign servers and user mappings
//! (foreigncmds.c). They don't affect typing, but foreign tables and
//! IMPORT FOREIGN SCHEMA name a server that must exist, and PG keeps the
//! names unique and the dependencies between them.

use typedpg_pg_query::ast::{
    self, CreateFdwStmt, CreateForeignServerStmt, CreateUserMappingStmt, DropUserMappingStmt,
    ImportForeignSchemaStmt, RoleSpec, RoleSpecType,
};

use super::DdlError;
use super::util::node_string;
use crate::oid::PgClassOid;
use crate::pg_catalog::PgCatalog;

/// The foreign-data catalogs (`pg_foreign_data_wrapper`,
/// `pg_foreign_server`, `pg_user_mapping`, `pg_foreign_table.ftserver`).
#[derive(Clone, Debug, Default)]
pub(crate) struct ForeignData {
    wrappers: Vec<String>,
    /// Each wrapper's OID (`pg_foreign_data_wrapper.oid`): the identity
    /// `pg_depend` rows name, which no rename changes.
    wrapper_oids: std::collections::HashMap<String, crate::oid::PgGenericOid>,
    /// The wrappers, by OID, with a CONNECTION function
    /// (`fdwconnection`, PostgreSQL 19).
    with_connection: std::collections::HashSet<crate::oid::PgGenericOid>,
    /// Each wrapper's HANDLER function (`fdwhandler`), by the wrapper's
    /// OID; a wrapper without one is absent.
    handlers: std::collections::HashMap<crate::oid::PgGenericOid, crate::oid::PgProcOid>,
    /// `(server, wrapper)`.
    servers: Vec<(String, String)>,
    /// `(user, server)`.
    user_mappings: Vec<(String, String)>,
    pub(crate) table_servers: std::collections::HashMap<PgClassOid, String>,
    /// The generic options each object holds (their names; the values
    /// don't matter until a wrapper validates them).
    options: std::collections::HashMap<OptionsOwner, Vec<String>>,
}

/// An object with generic options (`fdwoptions`, `srvoptions`,
/// `umoptions`, `ftoptions`, `attfdwoptions`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum OptionsOwner {
    Wrapper(String),
    Server(String),
    /// `(user, server)`.
    UserMapping(String, String),
    ForeignTable(PgClassOid),
    /// A foreign table's column, by attnum.
    Column(PgClassOid, i16),
}

impl ForeignData {
    /// Wrapper `name`'s OID.
    pub(crate) fn wrapper_oid(&self, name: &str) -> Option<crate::oid::PgGenericOid> {
        self.wrapper_oids.get(name).copied()
    }

    /// The wrapper whose OID is `oid`.
    pub(crate) fn wrapper_named(&self, oid: crate::oid::PgGenericOid) -> Option<String> {
        self.wrapper_oids
            .iter()
            .find(|(_, o)| **o == oid)
            .map(|(name, _)| name.clone())
    }

    pub(crate) fn wrapper_names(&self) -> impl Iterator<Item = String> + '_ {
        self.wrappers.iter().cloned()
    }

    /// The servers of wrapper `name`.
    pub(crate) fn servers_of(&self, name: &str) -> Vec<String> {
        self.servers
            .iter()
            .filter(|(_, w)| w == name)
            .map(|(s, _)| s.clone())
            .collect()
    }

    /// Forget wrapper `name` and its options (it has no servers left).
    pub(crate) fn forget_wrapper(&mut self, name: &str) {
        self.wrappers.retain(|w| w != name);
        if let Some(oid) = self.wrapper_oids.remove(name) {
            self.with_connection.remove(&oid);
            self.handlers.remove(&oid);
        }
        self.options
            .retain(|owner, _| !matches!(owner, OptionsOwner::Wrapper(w) if w == name));
    }

    /// Forget relation `relid`'s options and its columns' (DROP).
    pub(crate) fn forget_relation(&mut self, relid: PgClassOid) {
        self.table_servers.remove(&relid);
        self.options.retain(|owner, _| match owner {
            OptionsOwner::ForeignTable(r) | OptionsOwner::Column(r, _) => *r != relid,
            _ => true,
        });
    }

    /// Forget column `attnum` of `relid`'s options (DROP COLUMN).
    pub(crate) fn forget_column(&mut self, relid: PgClassOid, attnum: i16) {
        self.options.remove(&OptionsOwner::Column(relid, attnum));
    }

    fn rekey(&mut self, rename: impl Fn(&OptionsOwner) -> Option<OptionsOwner>) {
        let moved: Vec<(OptionsOwner, OptionsOwner)> = self
            .options
            .keys()
            .filter_map(|k| Some((k.clone(), rename(k)?)))
            .collect();
        for (old, new) in moved {
            if let Some(v) = self.options.remove(&old) {
                self.options.insert(new, v);
            }
        }
    }
}

fn fdw_missing(name: &str) -> DdlError {
    DdlError::TypeNotFound(format!("foreign-data wrapper \"{name}\" does not exist"))
}

fn server_missing(name: &str) -> DdlError {
    DdlError::TypeNotFound(format!("server \"{name}\" does not exist"))
}

/// CreateSubscription's `SERVER name` (PostgreSQL 19): the server must
/// exist and its wrapper have a CONNECTION function
/// (ForeignServerConnectionString).
pub(crate) fn check_subscription_server(interp: &PgCatalog, name: &str) -> Result<(), DdlError> {
    let data = &interp.foreign_data;
    let Some((_, wrapper)) = data.servers.iter().find(|(s, _)| s == name) else {
        return Err(server_missing(name));
    };
    if data
        .wrapper_oid(wrapper)
        .is_none_or(|oid| !data.with_connection.contains(&oid))
    {
        return Err(DdlError::UnsupportedDdl(format!(
            "foreign-data wrapper \"{wrapper}\" does not support subscription connections \
             (Foreign-data wrapper must be defined with CONNECTION specified.)"
        )));
    }
    Ok(())
}

/// get_foreign_server_oid.
pub(crate) fn check_server(interp: &PgCatalog, name: &str) -> Result<(), DdlError> {
    if interp.foreign_data.servers.iter().any(|(s, _)| s == name) {
        Ok(())
    } else {
        Err(server_missing(name))
    }
}

pub(crate) fn check_fdw(interp: &PgCatalog, name: &str) -> Result<(), DdlError> {
    if interp.foreign_data.wrappers.iter().any(|w| w == name) {
        Ok(())
    } else {
        Err(fdw_missing(name))
    }
}

/// What the analyzer knows of the `FdwRoutine` a foreign table's wrapper
/// hands out (GetFdwRoutineByServerId).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FdwRoutine {
    /// The wrapper has no handler: GetFdwRoutineByServerId fails.
    NoHandler,
    /// A handler whose routine is known (a bundled extension's).
    Known(KnownRoutine),
    /// A handler whose routine the analyzer can't see.
    Unknown,
}

/// The optional callbacks of a known `FdwRoutine` the executor checks
/// before using them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KnownRoutine {
    /// `ExecForeignInsert` (CheckValidResultRel).
    pub(crate) insert: bool,
    /// `ExecForeignUpdate`.
    pub(crate) update: bool,
    /// `ExecForeignDelete`.
    pub(crate) delete: bool,
    /// `ExecForeignTruncate` (truncate_check_rel).
    pub(crate) truncate: bool,
    /// `ImportForeignSchema` (ImportForeignSchema).
    pub(crate) import_schema: bool,
}

/// file_fdw_handler (contrib/file_fdw, PostgreSQL 19): scans and ANALYZE
/// only.
const FILE_FDW: KnownRoutine = KnownRoutine {
    insert: false,
    update: false,
    delete: false,
    truncate: false,
    import_schema: false,
};

/// `foreign-data wrapper "w" has no handler` — SQLSTATE 55000
/// (GetFdwRoutineByServerId, ImportForeignSchema).
pub(crate) fn no_handler_message(wrapper: &str) -> String {
    format!("foreign-data wrapper \"{wrapper}\" has no handler")
}

/// The routine handler `handler` returns, when the analyzer knows it: the
/// one a bundled extension's script created, by that extension's name and
/// its own (a C handler is known by its symbol, which the analyzer doesn't
/// keep, so a copy under another name stays unknown).
fn known_routine(interp: &PgCatalog, handler: crate::oid::PgProcOid) -> Option<KnownRoutine> {
    let proc = interp.pg_proc.get(&handler)?;
    let addr = super::depend::ObjectAddress::proc(handler);
    let extension = interp.pg_depend.iter().find_map(|d| {
        (d.classid == addr.classid
            && d.objid == addr.objid
            && d.deptype == crate::pg_catalog::DepType::Extension
            && d.refclassid == crate::pg_catalog::PG_EXTENSION_RELID)
            .then(|| crate::oid::PgExtensionOid::new(d.refobjid.get()))
            .flatten()
    })?;
    let extension = interp.pg_extension.get(&extension)?;
    match (extension.extname.as_str(), proc.proname.as_str()) {
        ("file_fdw", "file_fdw_handler") => Some(FILE_FDW),
        _ => None,
    }
}

/// The wrapper of server `server`, and its routine.
pub(crate) fn server_routine(interp: &PgCatalog, server: &str) -> Option<(String, FdwRoutine)> {
    let data = &interp.foreign_data;
    let (_, wrapper) = data.servers.iter().find(|(s, _)| s == server)?;
    let oid = data.wrapper_oid(wrapper)?;
    let routine = match data.handlers.get(&oid) {
        None => FdwRoutine::NoHandler,
        Some(&handler) => {
            known_routine(interp, handler).map_or(FdwRoutine::Unknown, FdwRoutine::Known)
        }
    };
    Some((wrapper.clone(), routine))
}

/// The wrapper of foreign table `relid`, and its routine
/// (GetFdwRoutineForRelation); `None` for any other relation.
pub(crate) fn table_routine(interp: &PgCatalog, relid: PgClassOid) -> Option<(String, FdwRoutine)> {
    let server = interp.foreign_data.table_servers.get(&relid)?;
    server_routine(interp, server)
}

/// The error GetFdwRoutineForRelation raises for foreign table `relid`,
/// whose wrapper has no handler.
pub(crate) fn no_handler_error(interp: &PgCatalog, relid: PgClassOid) -> Option<String> {
    match table_routine(interp, relid)? {
        (wrapper, FdwRoutine::NoHandler) => Some(no_handler_message(&wrapper)),
        _ => None,
    }
}

/// The error the planner raises planning a scan of relation `relid`
/// (`inh` unless ONLY): get_relation_info opens a foreign table's routine,
/// for the relation itself and — expanding an inheritance parent
/// (expand_inherited_rtentry, find_all_inheritors) — for every inheritor.
/// A partitioned table's partitions may be pruned first: not counted.
pub(crate) fn scan_no_handler_error(
    interp: &PgCatalog,
    relid: PgClassOid,
    inh: bool,
) -> Option<String> {
    let expands = inh
        && interp
            .pg_class
            .get(&relid)
            .is_some_and(|c| c.relkind != crate::pg_catalog::RelKind::Partitioned);
    if !expands {
        return no_handler_error(interp, relid);
    }
    super::tables::inherit::all_inheritors(interp, relid)
        .into_iter()
        .find_map(|r| no_handler_error(interp, r))
}

/// What a wrapper's function options set, where they name it.
#[derive(Default)]
struct FunctionOptions {
    /// `[NO] CONNECTION`: whether the wrapper has a connection function.
    connection: Option<bool>,
    /// `[NO] HANDLER`: the handler function, or none.
    handler: Option<Option<crate::oid::PgProcOid>>,
}

/// The HANDLER / VALIDATOR / CONNECTION functions (lookup_fdw_handler_func
/// / lookup_fdw_validator_func / lookup_fdw_connection_func), and what the
/// options set.
fn check_functions(
    interp: &PgCatalog,
    options: &[Option<typedpg_pg_query::ast::Node>],
) -> Result<FunctionOptions, DdlError> {
    let mut set = FunctionOptions::default();
    for opt in options {
        let Some(ast::Node::DefElem(de)) = opt.as_ref() else {
            continue;
        };
        match de.defname.as_str() {
            "connection" => set.connection = Some(de.arg.is_some()),
            // `NO HANDLER` has no argument.
            "handler" if de.arg.is_none() => set.handler = Some(None),
            _ => {}
        }
        let Some(ast::Node::List(l)) = de.arg.as_ref() else {
            continue;
        };
        let parts: Vec<&str> = l
            .items
            .iter()
            .filter_map(|n| node_string(n.as_ref()))
            .collect();
        let (schema, name) = match parts.as_slice() {
            [name] => (None, *name),
            [schema, name] => (Some(*schema), *name),
            _ => continue,
        };
        let candidates = interp.find_functions(schema, name);
        match de.defname.as_str() {
            "handler" => {
                let Some(p) = candidates.iter().find(|p| p.proargtypes.is_empty()) else {
                    return Err(DdlError::TypeNotFound(format!(
                        "function {name}() does not exist"
                    )));
                };
                if interp
                    .pg_type
                    .get(&p.prorettype)
                    .map(|t| t.typname.as_str())
                    != Some("fdw_handler")
                {
                    return Err(DdlError::Parse(format!(
                        "function {name} must return type fdw_handler"
                    )));
                }
                set.handler = Some(Some(p.oid));
            }
            "validator" => {
                // (text[], oid)
                let wanted = [
                    crate::oid::PgTypeOid::from_raw(1009),
                    crate::pg_catalog::oid::OID,
                ];
                if !candidates.iter().any(|p| p.proargtypes == wanted) {
                    return Err(DdlError::TypeNotFound(format!(
                        "function {name}(text[], oid) does not exist"
                    )));
                }
            }
            "connection" => {
                // lookup_fdw_connection_func (PostgreSQL 19): (oid, oid,
                // internal) returning text.
                let wanted = [
                    crate::pg_catalog::oid::OID,
                    crate::pg_catalog::oid::OID,
                    crate::oid::PgTypeOid::from_raw(2281), // internal
                ];
                let Some(p) = candidates.iter().find(|p| p.proargtypes == wanted) else {
                    return Err(DdlError::TypeNotFound(format!(
                        "function {name}(oid, oid, internal) does not exist"
                    )));
                };
                if p.prorettype != crate::pg_catalog::oid::TEXT {
                    return Err(DdlError::Parse(format!(
                        "function {} must return type text",
                        parts.join(".")
                    )));
                }
            }
            _ => {}
        }
    }
    Ok(set)
}

/// transformGenericOptions: `options` (an `OPTIONS (...)` list) applied
/// in order to the ones `owner` holds — a plain or ADD option must be new,
/// SET and DROP need it to be there — and stored.
pub(crate) fn apply_generic_options(
    interp: &mut PgCatalog,
    owner: OptionsOwner,
    options: &[Option<typedpg_pg_query::ast::Node>],
) -> Result<(), DdlError> {
    use typedpg_pg_query::ast::DefElemAction;
    let mut result = interp
        .foreign_data
        .options
        .get(&owner)
        .cloned()
        .unwrap_or_default();
    for opt in options {
        let Some(ast::Node::DefElem(de)) = opt.as_ref() else {
            continue;
        };
        let name = &de.defname;
        let found = result.iter().position(|n| n == name);
        match (de.defaction, found) {
            (DefElemAction::DefelemDrop, Some(i)) => {
                result.remove(i);
            }
            (DefElemAction::DefelemSet, Some(_)) => {}
            (DefElemAction::DefelemDrop | DefElemAction::DefelemSet, None) => {
                return Err(DdlError::TypeNotFound(format!(
                    "option \"{name}\" not found"
                )));
            }
            (_, Some(_)) => {
                return Err(DdlError::DuplicateObject(format!(
                    "option \"{name}\" provided more than once"
                )));
            }
            (_, None) => result.push(name.clone()),
        }
    }
    interp.foreign_data.options.insert(owner, result);
    Ok(())
}

pub fn create_fdw(interp: &mut PgCatalog, stmt: &CreateFdwStmt) -> Result<(), DdlError> {
    if interp.foreign_data.wrappers.contains(&stmt.fdwname) {
        return Err(DdlError::DuplicateObject(format!(
            "foreign-data wrapper \"{}\" already exists",
            stmt.fdwname
        )));
    }
    let functions = check_functions(interp, &stmt.func_options)?;
    apply_generic_options(
        interp,
        OptionsOwner::Wrapper(stmt.fdwname.clone()),
        &stmt.options,
    )?;
    let oid = crate::oid::PgGenericOid::from_nonzero(interp.alloc_oid()?);
    interp.foreign_data.wrappers.push(stmt.fdwname.clone());
    interp
        .foreign_data
        .wrapper_oids
        .insert(stmt.fdwname.clone(), oid);
    if functions.connection == Some(true) {
        interp.foreign_data.with_connection.insert(oid);
    }
    if let Some(Some(handler)) = functions.handler {
        interp.foreign_data.handlers.insert(oid, handler);
    }
    Ok(())
}

pub fn alter_fdw(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::ast::AlterFdwStmt,
) -> Result<(), DdlError> {
    check_fdw(interp, &stmt.fdwname)?;
    let functions = check_functions(interp, &stmt.func_options)?;
    apply_generic_options(
        interp,
        OptionsOwner::Wrapper(stmt.fdwname.clone()),
        &stmt.options,
    )?;
    let Some(oid) = interp.foreign_data.wrapper_oid(&stmt.fdwname) else {
        return Ok(());
    };
    let fd = &mut interp.foreign_data;
    match functions.connection {
        Some(true) => {
            fd.with_connection.insert(oid);
        }
        Some(false) => {
            fd.with_connection.remove(&oid);
        }
        None => {}
    }
    // AlterForeignDataWrapper: a new handler, or none, only warns that the
    // existing foreign tables' behavior changes.
    match functions.handler {
        Some(Some(handler)) => {
            fd.handlers.insert(oid, handler);
        }
        Some(None) => {
            fd.handlers.remove(&oid);
        }
        None => {}
    }
    Ok(())
}

pub fn create_server(
    interp: &mut PgCatalog,
    stmt: &CreateForeignServerStmt,
) -> Result<(), DdlError> {
    if interp
        .foreign_data
        .servers
        .iter()
        .any(|(s, _)| *s == stmt.servername)
    {
        if stmt.if_not_exists {
            return Ok(());
        }
        return Err(DdlError::DuplicateObject(format!(
            "server \"{}\" already exists",
            stmt.servername
        )));
    }
    check_fdw(interp, &stmt.fdwname)?;
    apply_generic_options(
        interp,
        OptionsOwner::Server(stmt.servername.clone()),
        &stmt.options,
    )?;
    interp
        .foreign_data
        .servers
        .push((stmt.servername.clone(), stmt.fdwname.clone()));
    Ok(())
}

pub fn alter_server(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::ast::AlterForeignServerStmt,
) -> Result<(), DdlError> {
    check_server(interp, &stmt.servername)?;
    apply_generic_options(
        interp,
        OptionsOwner::Server(stmt.servername.clone()),
        &stmt.options,
    )
}

/// The role a user mapping is for, as PG names it in messages.
fn role_name(role: Option<&RoleSpec>) -> String {
    match role.map(|r| r.roletype) {
        Some(RoleSpecType::RolespecPublic) | None => "public".to_owned(),
        Some(RoleSpecType::RolespecCstring) => role.map(|r| r.rolename.clone()).unwrap_or_default(),
        Some(_) => "current_user".to_owned(),
    }
}

pub fn create_user_mapping(
    interp: &mut PgCatalog,
    stmt: &CreateUserMappingStmt,
) -> Result<(), DdlError> {
    check_server(interp, &stmt.servername)?;
    let user = role_name(stmt.user.as_deref());
    let key = (user.clone(), stmt.servername.clone());
    if interp.foreign_data.user_mappings.contains(&key) {
        if stmt.if_not_exists {
            return Ok(());
        }
        return Err(DdlError::DuplicateObject(format!(
            "user mapping for \"{user}\" already exists for server \"{}\"",
            stmt.servername
        )));
    }
    apply_generic_options(
        interp,
        OptionsOwner::UserMapping(key.0.clone(), key.1.clone()),
        &stmt.options,
    )?;
    interp.foreign_data.user_mappings.push(key);
    Ok(())
}

pub fn alter_user_mapping(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::ast::AlterUserMappingStmt,
) -> Result<(), DdlError> {
    check_server(interp, &stmt.servername)?;
    let user = role_name(stmt.user.as_deref());
    if !interp
        .foreign_data
        .user_mappings
        .contains(&(user.clone(), stmt.servername.clone()))
    {
        return Err(DdlError::TypeNotFound(format!(
            "user mapping for \"{user}\" does not exist for server \"{}\"",
            stmt.servername
        )));
    }
    apply_generic_options(
        interp,
        OptionsOwner::UserMapping(user, stmt.servername.clone()),
        &stmt.options,
    )
}

pub fn drop_user_mapping(
    interp: &mut PgCatalog,
    stmt: &DropUserMappingStmt,
) -> Result<(), DdlError> {
    if check_server(interp, &stmt.servername).is_err() {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(server_missing(&stmt.servername));
    }
    let user = role_name(stmt.user.as_deref());
    let key = (user.clone(), stmt.servername.clone());
    let before = interp.foreign_data.user_mappings.len();
    interp.foreign_data.user_mappings.retain(|m| *m != key);
    interp
        .foreign_data
        .options
        .remove(&OptionsOwner::UserMapping(key.0.clone(), key.1.clone()));
    if interp.foreign_data.user_mappings.len() == before && !stmt.missing_ok {
        return Err(DdlError::TypeNotFound(format!(
            "user mapping for \"{user}\" does not exist for server \"{}\"",
            stmt.servername
        )));
    }
    Ok(())
}

/// IMPORT FOREIGN SCHEMA: the server and the local schema must exist, and
/// the server's wrapper have a handler that supports IMPORT. The imported
/// tables come from the remote server, which the analyzer can't see.
pub fn import_foreign_schema(
    interp: &PgCatalog,
    stmt: &ImportForeignSchemaStmt,
) -> Result<(), DdlError> {
    check_server(interp, &stmt.server_name)?;
    if interp.namespace_oid(&stmt.local_schema).is_none() {
        return Err(DdlError::TableNotFound(format!(
            "schema \"{}\" does not exist",
            stmt.local_schema
        )));
    }
    // ImportForeignSchema: the wrapper's handler, then its
    // ImportForeignSchema callback (ERRCODE_FDW_NO_SCHEMAS).
    match server_routine(interp, &stmt.server_name) {
        Some((wrapper, FdwRoutine::NoHandler)) => {
            Err(DdlError::UnsupportedDdl(no_handler_message(&wrapper)))
        }
        Some((wrapper, FdwRoutine::Known(routine))) if !routine.import_schema => {
            Err(DdlError::UnsupportedDdl(format!(
                "foreign-data wrapper \"{wrapper}\" does not support IMPORT FOREIGN SCHEMA"
            )))
        }
        _ => Ok(()),
    }
}

/// DROP FOREIGN DATA WRAPPER / DROP SERVER, with their dependents.
pub(crate) fn drop_foreign_object(
    interp: &mut PgCatalog,
    wrapper: bool,
    obj_node: Option<&typedpg_pg_query::ast::Node>,
    missing_ok: bool,
    cascade: bool,
) -> Result<(), DdlError> {
    let Some(name) = node_string(obj_node).map(str::to_owned) else {
        return Ok(());
    };
    let servers: Vec<String> = if wrapper {
        if check_fdw(interp, &name).is_err() {
            return if missing_ok {
                Ok(())
            } else {
                Err(fdw_missing(&name))
            };
        }
        interp
            .foreign_data
            .servers
            .iter()
            .filter(|(_, w)| *w == name)
            .map(|(s, _)| s.clone())
            .collect()
    } else {
        if check_server(interp, &name).is_err() {
            return if missing_ok {
                Ok(())
            } else {
                Err(server_missing(&name))
            };
        }
        vec![name.clone()]
    };
    let what = if wrapper {
        // An extension's member goes only with it.
        let object = super::depend::NamedObject::ForeignDataWrapper { name: name.clone() };
        if let Some(addr) = super::depend::named_address_of(interp, &object) {
            super::depend::check_not_owned(interp, addr, &format!("foreign-data wrapper {name}"))?;
        }
        "foreign-data wrapper"
    } else {
        "server"
    };
    drop_servers(
        interp,
        &servers,
        wrapper,
        cascade,
        &format!("{what} {name}"),
    )?;
    if wrapper {
        interp.foreign_data.forget_wrapper(&name);
    }
    Ok(())
}

/// Drop `servers` with their user mappings and foreign tables, for the
/// DROP of `dropped_desc` — their wrapper's when `of_wrapper`, then the
/// servers themselves are dependents too: any dependent needs CASCADE.
pub(crate) fn drop_servers(
    interp: &mut PgCatalog,
    servers: &[String],
    of_wrapper: bool,
    cascade: bool,
    dropped_desc: &str,
) -> Result<(), DdlError> {
    super::cluster::check_no_subscription_on(interp, servers)?;
    let tables: Vec<PgClassOid> = interp
        .foreign_data
        .table_servers
        .iter()
        .filter(|(_, s)| servers.contains(s))
        .map(|(t, _)| *t)
        .collect();
    let has_mappings = interp
        .foreign_data
        .user_mappings
        .iter()
        .any(|(_, s)| servers.contains(s));
    let has_dependents = !tables.is_empty() || has_mappings || (of_wrapper && !servers.is_empty());
    if has_dependents && !cascade {
        return Err(DdlError::DependencyError(format!(
            "cannot drop {dropped_desc} because other objects depend on it"
        )));
    }
    for table in tables {
        super::drop::drop_relation_by_oid(interp, table);
    }
    let fd = &mut interp.foreign_data;
    fd.user_mappings.retain(|(_, s)| !servers.contains(s));
    fd.servers.retain(|(s, _)| !servers.contains(s));
    fd.options.retain(|owner, _| match owner {
        OptionsOwner::Server(s) | OptionsOwner::UserMapping(_, s) => !servers.contains(s),
        _ => true,
    });
    Ok(())
}

/// ALTER FOREIGN DATA WRAPPER / SERVER ... RENAME TO.
pub(crate) fn rename_foreign_object(
    interp: &mut PgCatalog,
    wrapper: bool,
    stmt: &typedpg_pg_query::ast::RenameStmt,
) -> Result<(), DdlError> {
    let Some(old) = node_string(stmt.object.as_ref()).map(str::to_owned) else {
        return Ok(());
    };
    let new = stmt.newname.clone();
    let fd = &mut interp.foreign_data;
    if wrapper {
        if !fd.wrappers.contains(&old) {
            return Err(fdw_missing(&old));
        }
        if fd.wrappers.contains(&new) {
            return Err(DdlError::DuplicateObject(format!(
                "foreign-data wrapper \"{new}\" already exists"
            )));
        }
        for w in fd.wrappers.iter_mut().filter(|w| **w == old) {
            w.clone_from(&new);
        }
        if let Some(oid) = fd.wrapper_oids.remove(&old) {
            fd.wrapper_oids.insert(new.clone(), oid);
        }
        for (_, w) in fd.servers.iter_mut().filter(|(_, w)| *w == old) {
            w.clone_from(&new);
        }
        fd.rekey(|k| match k {
            OptionsOwner::Wrapper(w) if *w == old => Some(OptionsOwner::Wrapper(new.clone())),
            _ => None,
        });
    } else {
        if !fd.servers.iter().any(|(s, _)| *s == old) {
            return Err(server_missing(&old));
        }
        if fd.servers.iter().any(|(s, _)| *s == new) {
            return Err(DdlError::DuplicateObject(format!(
                "server \"{new}\" already exists"
            )));
        }
        for (s, _) in fd.servers.iter_mut().filter(|(s, _)| *s == old) {
            s.clone_from(&new);
        }
        for (_, s) in fd.user_mappings.iter_mut().filter(|(_, s)| *s == old) {
            s.clone_from(&new);
        }
        for s in fd.table_servers.values_mut().filter(|s| **s == old) {
            s.clone_from(&new);
        }
        fd.rekey(|k| match k {
            OptionsOwner::Server(s) if *s == old => Some(OptionsOwner::Server(new.clone())),
            OptionsOwner::UserMapping(u, s) if *s == old => {
                Some(OptionsOwner::UserMapping(u.clone(), new.clone()))
            }
            _ => None,
        });
        super::cluster::rename_subscription_server(interp, &old, &new);
    }
    Ok(())
}
