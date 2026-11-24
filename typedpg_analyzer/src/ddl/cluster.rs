//! Objects outside the schema that migrations can still name: tablespaces,
//! subscriptions and large objects. None affects typing, but COMMENT,
//! GRANT and DROP look them up (get_object_address), so the catalog keeps
//! the ones the migrations create next to the built-in tablespaces.

use typedpg_pg_query::ast::{
    self, CreateSubscriptionStmt, CreateTableSpaceStmt, DropSubscriptionStmt, DropTableSpaceStmt,
    FuncCall,
};

use super::DdlError;
use crate::pg_catalog::PgCatalog;

#[derive(Clone, Debug, Default)]
pub(crate) struct ClusterObjects {
    /// Tablespaces created by migrations (`pg_default` and `pg_global`
    /// always exist).
    tablespaces: Vec<String>,
    subscriptions: Vec<Subscription>,
    /// Large objects created with a constant OID (`lo_create(1234)`).
    large_objects: Vec<u32>,
}

impl ClusterObjects {
    /// Each subscription's name and foreign server (for the catalog
    /// oracle).
    #[cfg(feature = "pg_sanity")]
    pub(crate) fn subscription_servers(&self) -> impl Iterator<Item = (&str, Option<&str>)> + '_ {
        self.subscriptions
            .iter()
            .map(|s| (s.name.as_str(), s.server.as_deref()))
    }
}

/// What the analyzer keeps of a `pg_subscription` row.
#[derive(Clone, Debug)]
struct Subscription {
    name: String,
    /// The foreign server it connects through (PostgreSQL 19's `SERVER`):
    /// pg_depend records the dependency, so the server can't be dropped
    /// before the subscription.
    server: Option<String>,
    /// Whether `subslotname` is set (not `slot_name = NONE`).
    slot: bool,
    enabled: bool,
}

fn subscription_mut<'a>(interp: &'a mut PgCatalog, name: &str) -> Option<&'a mut Subscription> {
    interp
        .cluster_objects
        .subscriptions
        .iter_mut()
        .find(|s| s.name == name)
}

/// get_tablespace_oid.
pub(crate) fn tablespace_exists(interp: &PgCatalog, name: &str) -> bool {
    matches!(name, "pg_default" | "pg_global")
        || interp.cluster_objects.tablespaces.iter().any(|t| t == name)
}

/// get_subscription_oid.
pub(crate) fn subscription_exists(interp: &PgCatalog, name: &str) -> bool {
    interp
        .cluster_objects
        .subscriptions
        .iter()
        .any(|s| s.name == name)
}

/// DROP SUBSCRIPTION's PreventInTransactionBlock: a subscription with a
/// replication slot drops the slot too, which can't be rolled back.
pub(crate) fn subscription_has_slot(interp: &PgCatalog, name: &str) -> bool {
    interp
        .cluster_objects
        .subscriptions
        .iter()
        .any(|s| s.name == name && s.slot)
}

/// findDependentObjects (PostgreSQL 19): a subscription connecting through
/// one of `servers` stops any DROP reaching it, CASCADE or not — it lives in
/// a shared catalog, which doDeletion can't delete from. The error names the
/// first such server and subscription.
pub(crate) fn check_no_subscription_on(
    interp: &PgCatalog,
    servers: &[String],
) -> Result<(), DdlError> {
    let found = interp.cluster_objects.subscriptions.iter().find_map(|sub| {
        let server = sub.server.as_ref().filter(|s| servers.contains(s))?;
        Some((&sub.name, server))
    });
    match found {
        Some((sub, server)) => Err(DdlError::DependencyError(format!(
            "cannot drop server {server} because subscription {sub} depends on it \
             (Drop subscription {sub} first.)"
        ))),
        None => Ok(()),
    }
}

/// ALTER SERVER ... RENAME TO: the subscriptions follow it.
pub(crate) fn rename_subscription_server(interp: &mut PgCatalog, old: &str, new: &str) {
    for sub in &mut interp.cluster_objects.subscriptions {
        if sub.server.as_deref() == Some(old) {
            sub.server = Some(new.to_owned());
        }
    }
}

/// ALTER SUBSCRIPTION ... RENAME TO (AlterObjectRename_internal).
pub(crate) fn rename_subscription(
    interp: &mut PgCatalog,
    stmt: &ast::RenameStmt,
) -> Result<(), DdlError> {
    let Some(old) = super::util::node_string(stmt.object.as_ref()).map(str::to_owned) else {
        return Ok(());
    };
    check_subscription(interp, &old)?;
    if subscription_exists(interp, &stmt.newname) {
        return Err(DdlError::DuplicateObject(format!(
            "subscription \"{}\" already exists",
            stmt.newname
        )));
    }
    if let Some(sub) = subscription_mut(interp, &old) {
        sub.name.clone_from(&stmt.newname);
    }
    Ok(())
}

/// The options parse_subscription_options reads that the analyzer keeps
/// track of — every other one is left to PostgreSQL.
#[derive(Default)]
pub(crate) struct SubOpts {
    connect: Option<bool>,
    enabled: Option<bool>,
    create_slot: Option<bool>,
    copy_data: Option<bool>,
    /// `Some(None)`: `slot_name = NONE`.
    slot_name: Option<Option<String>>,
}

impl SubOpts {
    /// The defaults CREATE SUBSCRIPTION applies (`connect = false` turns
    /// enabled, create_slot and copy_data off): `(enabled, create_slot,
    /// has a slot name)`.
    fn resolved(&self) -> (bool, bool, bool) {
        let connect = self.connect.unwrap_or(true);
        let enabled = self.enabled.unwrap_or(true) && connect;
        let create_slot = self.create_slot.unwrap_or(true) && connect;
        let slot = !matches!(self.slot_name, Some(None));
        (enabled, create_slot, slot)
    }
}

/// parse_subscription_options for the options in `supported` (by name):
/// repeated ones, Boolean values, slot names and the combinations it
/// refuses.
pub(crate) fn parse_subscription_options(
    options: &[Option<ast::Node>],
    supported: &[&str],
) -> Result<SubOpts, DdlError> {
    let mut opts = SubOpts::default();
    for opt in options {
        let Some(ast::Node::DefElem(de)) = opt.as_ref() else {
            continue;
        };
        let name = de.defname.as_str();
        if !supported.contains(&name) {
            continue;
        }
        let slot = match name {
            "connect" => &mut opts.connect,
            "enabled" => &mut opts.enabled,
            "create_slot" => &mut opts.create_slot,
            "copy_data" => &mut opts.copy_data,
            "slot_name" => {
                if opts.slot_name.is_some() {
                    return Err(DdlError::Parse("conflicting or redundant options".into()));
                }
                let value = de
                    .arg
                    .as_ref()
                    .and_then(super::session::string_arg)
                    .unwrap_or_default();
                opts.slot_name = Some(if value == "none" {
                    None
                } else {
                    validate_slot_name(&value)?;
                    Some(value)
                });
                continue;
            }
            _ => continue,
        };
        if slot.is_some() {
            return Err(DdlError::Parse("conflicting or redundant options".into()));
        }
        *slot = Some(super::session::def_get_boolean(de)?);
    }
    let exclusive = |a: &str, b: &str| {
        Err(DdlError::Parse(format!(
            "{a} and {b} are mutually exclusive options"
        )))
    };
    if supported.contains(&"connect") && opts.connect == Some(false) {
        if opts.enabled == Some(true) {
            return exclusive("connect = false", "enabled = true");
        }
        if opts.create_slot == Some(true) {
            return exclusive("connect = false", "create_slot = true");
        }
        if opts.copy_data == Some(true) {
            return exclusive("connect = false", "copy_data = true");
        }
    }
    if matches!(opts.slot_name, Some(None)) {
        let connect = !supported.contains(&"connect") || opts.connect != Some(false);
        let default_on = |o: Option<bool>, supported: bool| supported && connect && o.is_none();
        if opts.enabled == Some(true) {
            return exclusive("slot_name = NONE", "enabled = true");
        }
        if default_on(opts.enabled, supported.contains(&"enabled")) {
            return Err(DdlError::Parse(
                "subscription with slot_name = NONE must also set enabled = false".into(),
            ));
        }
        if opts.create_slot == Some(true) {
            return exclusive("slot_name = NONE", "create_slot = true");
        }
        if default_on(opts.create_slot, supported.contains(&"create_slot")) {
            return Err(DdlError::Parse(
                "subscription with slot_name = NONE must also set create_slot = false".into(),
            ));
        }
    }
    Ok(opts)
}

/// The options CREATE SUBSCRIPTION reads, as far as the analyzer does.
const CREATE_OPTIONS: &[&str] = &[
    "connect",
    "enabled",
    "create_slot",
    "copy_data",
    "slot_name",
];

/// CREATE SUBSCRIPTION's PreventInTransactionBlock: creating the slot can't
/// be rolled back. `None` when the options are invalid (the statement fails
/// on them first).
pub(crate) fn creates_slot(stmt: &CreateSubscriptionStmt) -> Option<bool> {
    let opts = parse_subscription_options(&stmt.options, CREATE_OPTIONS).ok()?;
    Some(opts.resolved().1)
}

/// ReplicationSlotValidateName.
fn validate_slot_name(name: &str) -> Result<(), DdlError> {
    let msg = if name.is_empty() {
        format!("replication slot name \"{name}\" is too short")
    } else if name.len() >= 64 {
        format!("replication slot name \"{name}\" is too long")
    } else if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        format!(
            "replication slot name \"{name}\" contains invalid character (Replication slot \
             names may only contain lower case letters, numbers, and the underscore character.)"
        )
    } else if name == "pg_conflict_detection" {
        format!(
            "replication slot name \"{name}\" is reserved (The name \"pg_conflict_detection\" \
             is reserved for the conflict detection slot.)"
        )
    } else {
        return Ok(());
    };
    Err(DdlError::Parse(msg))
}

/// LargeObjectExists.
pub(crate) fn large_object_exists(interp: &PgCatalog, oid: u32) -> bool {
    interp.cluster_objects.large_objects.contains(&oid)
}

pub(crate) fn create_tablespace(
    interp: &mut PgCatalog,
    stmt: &CreateTableSpaceStmt,
) -> Result<(), DdlError> {
    // CreateTableSpace (PostgreSQL 19).
    if stmt.location.contains('\'') {
        return Err(DdlError::Parse(
            "tablespace location cannot contain single quotes".into(),
        ));
    }
    if stmt.tablespacename.contains(['\n', '\r']) {
        return Err(DdlError::Parse(format!(
            "tablespace name \"{}\" contains a newline or carriage return character",
            stmt.tablespacename
        )));
    }
    // An empty location is an in-place tablespace, which only
    // allow_in_place_tablespaces permits: the analyzer doesn't track that
    // developer setting, so it takes an empty one as allowed.
    if !stmt.location.is_empty() && !stmt.location.starts_with('/') {
        return Err(DdlError::Parse(
            "tablespace location must be an absolute path".into(),
        ));
    }
    if is_reserved_name(interp, &stmt.tablespacename) {
        return Err(reserved_tablespace_name(&stmt.tablespacename));
    }
    if tablespace_exists(interp, &stmt.tablespacename) {
        return Err(DdlError::DuplicateObject(format!(
            "tablespace \"{}\" already exists",
            stmt.tablespacename
        )));
    }
    interp
        .cluster_objects
        .tablespaces
        .push(stmt.tablespacename.clone());
    Ok(())
}

/// `!allowSystemTableMods && IsReservedName(name)` (catalog.c): the `pg_`
/// prefix is the system's.
fn is_reserved_name(interp: &PgCatalog, name: &str) -> bool {
    !interp.allow_system_table_mods.get() && name.starts_with("pg_")
}

fn reserved_tablespace_name(name: &str) -> DdlError {
    DdlError::Parse(format!(
        "unacceptable tablespace name \"{name}\" (The prefix \"pg_\" is reserved for system \
         tablespaces.)"
    ))
}

/// ALTER TABLESPACE ... RENAME TO (RenameTableSpace, PostgreSQL 19): the
/// new name isn't reserved, has no newline or carriage return and is free.
/// A tablespace the migrations didn't create is assumed to exist, as for
/// placement (a DBA creates them); it goes by its new name afterwards.
/// (`pg_default` and `pg_global` can be renamed too, but the catalog keeps
/// their names.)
pub(crate) fn rename_tablespace(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::ast::RenameStmt,
) -> Result<(), DdlError> {
    let (old, new) = (&stmt.subname, &stmt.newname);
    if is_reserved_name(interp, new) {
        return Err(reserved_tablespace_name(new));
    }
    if new.contains(['\n', '\r']) {
        return Err(DdlError::Parse(format!(
            "tablespace name \"{new}\" contains a newline or carriage return character"
        )));
    }
    if tablespace_exists(interp, new) {
        return Err(DdlError::DuplicateObject(format!(
            "tablespace \"{new}\" already exists"
        )));
    }
    let objects = &mut interp.cluster_objects.tablespaces;
    match objects.iter_mut().find(|t| *t == old) {
        Some(t) => new.clone_into(t),
        None if !matches!(old.as_str(), "pg_default" | "pg_global") => objects.push(new.clone()),
        None => {}
    }
    Ok(())
}

/// ALTER DATABASE ... RENAME TO (RenameDatabase, PostgreSQL 19): no
/// newline or carriage return in the new name, checked before the database
/// is looked up. Databases are cluster objects the analyzer doesn't see:
/// the one named is assumed to exist and the new name to be free.
pub(crate) fn rename_database(stmt: &typedpg_pg_query::ast::RenameStmt) -> Result<(), DdlError> {
    if stmt.newname.contains(['\n', '\r']) {
        return Err(DdlError::Parse(format!(
            "database name \"{}\" contains a newline or carriage return character",
            stmt.newname
        )));
    }
    Ok(())
}

pub(crate) fn drop_tablespace(
    interp: &mut PgCatalog,
    stmt: &DropTableSpaceStmt,
) -> Result<(), DdlError> {
    let objects = &mut interp.cluster_objects.tablespaces;
    let before = objects.len();
    objects.retain(|t| *t != stmt.tablespacename);
    if objects.len() == before && !stmt.missing_ok {
        return Err(DdlError::TypeNotFound(format!(
            "tablespace \"{}\" does not exist",
            stmt.tablespacename
        )));
    }
    Ok(())
}

pub(crate) fn create_subscription(
    interp: &mut PgCatalog,
    stmt: &CreateSubscriptionStmt,
) -> Result<(), DdlError> {
    let opts = parse_subscription_options(&stmt.options, CREATE_OPTIONS)?;
    if subscription_exists(interp, &stmt.subname) {
        return Err(DdlError::DuplicateObject(format!(
            "subscription \"{}\" already exists",
            stmt.subname
        )));
    }
    // PostgreSQL 19's `SERVER name`.
    if !stmt.servername.is_empty() {
        super::fdw::check_subscription_server(interp, &stmt.servername)?;
    }
    let (enabled, _, slot) = opts.resolved();
    interp.cluster_objects.subscriptions.push(Subscription {
        name: stmt.subname.clone(),
        server: (!stmt.servername.is_empty()).then(|| stmt.servername.clone()),
        slot,
        enabled,
    });
    Ok(())
}

/// get_subscription_oid.
pub(crate) fn check_subscription(interp: &PgCatalog, name: &str) -> Result<(), DdlError> {
    if subscription_exists(interp, name) {
        Ok(())
    } else {
        Err(DdlError::TypeNotFound(format!(
            "subscription \"{name}\" does not exist"
        )))
    }
}

pub(crate) fn alter_subscription(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::ast::AlterSubscriptionStmt,
) -> Result<(), DdlError> {
    use typedpg_pg_query::ast::AlterSubscriptionType as Kind;
    let supported: &[&str] = match stmt.kind {
        Kind::AlterSubscriptionOptions => &["slot_name"],
        Kind::AlterSubscriptionEnabled => &["enabled"],
        Kind::AlterSubscriptionSetPublication
        | Kind::AlterSubscriptionAddPublication
        | Kind::AlterSubscriptionDropPublication
        | Kind::AlterSubscriptionRefreshPublication => &["copy_data"],
        _ => &[],
    };
    check_subscription(interp, &stmt.subname)?;
    let opts = parse_subscription_options(&stmt.options, supported)?;
    let Some(sub) = subscription_mut(interp, &stmt.subname) else {
        return Ok(());
    };
    match stmt.kind {
        Kind::AlterSubscriptionOptions => {
            if let Some(slot_name) = &opts.slot_name {
                if sub.enabled && slot_name.is_none() {
                    return Err(DdlError::UnsupportedDdl(
                        "cannot set slot_name = NONE for enabled subscription".into(),
                    ));
                }
                sub.slot = slot_name.is_some();
            }
        }
        Kind::AlterSubscriptionEnabled => {
            let enabled = opts.enabled.unwrap_or(true);
            if enabled && !sub.slot {
                return Err(DdlError::UnsupportedDdl(
                    "cannot enable subscription that does not have a slot name".into(),
                ));
            }
            sub.enabled = enabled;
        }
        // PostgreSQL 19's `ALTER SUBSCRIPTION ... SERVER name` moves the
        // dependency to the new server; `CONNECTION` removes it.
        Kind::AlterSubscriptionServer => {
            super::fdw::check_subscription_server(interp, &stmt.servername)?;
            if let Some(sub) = subscription_mut(interp, &stmt.subname) {
                sub.server = Some(stmt.servername.clone());
            }
        }
        Kind::AlterSubscriptionConnection => sub.server = None,
        _ => {}
    }
    Ok(())
}

pub(crate) fn drop_subscription(
    interp: &mut PgCatalog,
    stmt: &DropSubscriptionStmt,
) -> Result<(), DdlError> {
    let Some(sub) = interp
        .cluster_objects
        .subscriptions
        .iter()
        .find(|s| s.name == stmt.subname)
    else {
        if stmt.missing_ok {
            return Ok(());
        }
        return Err(DdlError::TypeNotFound(format!(
            "subscription \"{}\" does not exist",
            stmt.subname
        )));
    };
    // DropSubscription (PostgreSQL 19): to drop its slot, a subscription
    // through a server builds its connection string from the server's
    // wrapper (construct_subserver_conninfo →
    // ForeignServerConnectionString), which needs a CONNECTION function.
    // (PreventInTransactionBlock, before, is txblock's.)
    if sub.slot
        && let Some(server) = &sub.server
    {
        super::fdw::check_subscription_server(interp, server)?;
    }
    interp
        .cluster_objects
        .subscriptions
        .retain(|s| s.name != stmt.subname);
    Ok(())
}

/// The large-object calls of a top-level `SELECT` with a constant OID:
/// `lo_create(oid)`, `lo_from_bytea(oid, data)` and `lo_import(file, oid)`
/// create the object, `lo_unlink(oid)` removes it. (An OID of 0, or one
/// computed at run time, names an object the analyzer can't know.)
pub(crate) fn large_object_calls(interp: &mut PgCatalog, fc: &FuncCall) {
    let name: Vec<&str> = fc
        .funcname
        .iter()
        .filter_map(|n| super::util::node_string(n.as_ref()))
        .collect();
    let name = match name.as_slice() {
        [name] | ["pg_catalog", name] => *name,
        _ => return,
    };
    let constant = |n: Option<&typedpg_pg_query::ast::Node>| match n? {
        ast::Node::AConst(c) => match c.val.as_ref()? {
            ast::AConstValue::Integer(i) => u32::try_from(i.ival).ok().filter(|&v| v != 0),
            ast::AConstValue::Float(f) => f.fval.parse::<u32>().ok().filter(|&v| v != 0),
            _ => None,
        },
        _ => None,
    };
    let objects = &mut interp.cluster_objects.large_objects;
    match name {
        "lo_create" | "lo_from_bytea" => {
            if let Some(oid) = constant(fc.args.first().and_then(Option::as_ref)) {
                objects.push(oid);
            }
        }
        "lo_import" => {
            if let Some(oid) = constant(fc.args.get(1).and_then(Option::as_ref)) {
                objects.push(oid);
            }
        }
        "lo_unlink" => {
            if let Some(oid) = constant(fc.args.first().and_then(Option::as_ref)) {
                objects.retain(|&o| o != oid);
            }
        }
        _ => {}
    }
}
