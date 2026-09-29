//! GRANT / REVOKE on objects and ALTER DEFAULT PRIVILEGES. Privileges don't
//! affect static analysis, but PG resolves every target object
//! (`objectNamesToOids`), checks the privileges against the object kind
//! (ExecuteGrantStmt) and then the objects themselves (ExecGrant_Relation,
//! ExecGrant_Type_check, ExecGrant_Language_check, merge_acl_with_grant),
//! so a GRANT naming a missing object or a privilege it doesn't have fails
//! the migration.
//!
//! Grantee roles are assumed to exist unless no role can bear the name
//! (`public`, a reserved `pg_` name): roles live in the cluster, outside
//! what the migrations build.

use typedpg_pg_query::protobuf::{GrantStmt, GrantTargetType, ObjectType, RoleSpecType, node};

use super::DdlError;
use super::util::node_string;
use crate::oid::PgClassOid;
use crate::pg_catalog::{PgCatalog, RelKind};

/// `ACL_*` privilege bits (acl.h / parsenodes.h).
mod bits {
    pub const INSERT: u32 = 1 << 0;
    pub const SELECT: u32 = 1 << 1;
    pub const UPDATE: u32 = 1 << 2;
    pub const DELETE: u32 = 1 << 3;
    pub const TRUNCATE: u32 = 1 << 4;
    pub const REFERENCES: u32 = 1 << 5;
    pub const TRIGGER: u32 = 1 << 6;
    pub const EXECUTE: u32 = 1 << 7;
    pub const USAGE: u32 = 1 << 8;
    pub const CREATE: u32 = 1 << 9;
    pub const CREATE_TEMP: u32 = 1 << 10;
    pub const CONNECT: u32 = 1 << 11;
    pub const SET: u32 = 1 << 12;
    pub const ALTER_SYSTEM: u32 = 1 << 13;
    pub const MAINTAIN: u32 = 1 << 14;

    pub const COLUMN: u32 = INSERT | SELECT | UPDATE | REFERENCES;
    pub const RELATION: u32 =
        INSERT | SELECT | UPDATE | DELETE | TRUNCATE | REFERENCES | TRIGGER | MAINTAIN;
    pub const SEQUENCE: u32 = USAGE | SELECT | UPDATE;
    pub const DATABASE: u32 = CREATE | CREATE_TEMP | CONNECT;
    pub const FDW: u32 = USAGE;
    pub const FOREIGN_SERVER: u32 = USAGE;
    pub const FUNCTION: u32 = EXECUTE;
    pub const LANGUAGE: u32 = USAGE;
    pub const LARGEOBJECT: u32 = SELECT | UPDATE;
    pub const PARAMETER_ACL: u32 = SET | ALTER_SYSTEM;
    pub const SCHEMA: u32 = USAGE | CREATE;
    pub const TABLESPACE: u32 = CREATE;
    pub const TYPE: u32 = USAGE;
}

/// string_to_privilege.
fn privilege(name: &str) -> Result<u32, DdlError> {
    Ok(match name {
        "insert" => bits::INSERT,
        "select" => bits::SELECT,
        "update" => bits::UPDATE,
        "delete" => bits::DELETE,
        "truncate" => bits::TRUNCATE,
        "references" => bits::REFERENCES,
        "trigger" => bits::TRIGGER,
        "execute" => bits::EXECUTE,
        "usage" => bits::USAGE,
        "create" => bits::CREATE,
        "temporary" | "temp" => bits::CREATE_TEMP,
        "connect" => bits::CONNECT,
        "set" => bits::SET,
        "alter system" => bits::ALTER_SYSTEM,
        "maintain" => bits::MAINTAIN,
        other => {
            return Err(DdlError::Parse(format!(
                "unrecognized privilege type \"{other}\""
            )));
        }
    })
}

/// privilege_to_string.
fn privilege_name(bit: u32) -> &'static str {
    match bit {
        bits::INSERT => "INSERT",
        bits::SELECT => "SELECT",
        bits::UPDATE => "UPDATE",
        bits::DELETE => "DELETE",
        bits::TRUNCATE => "TRUNCATE",
        bits::REFERENCES => "REFERENCES",
        bits::TRIGGER => "TRIGGER",
        bits::EXECUTE => "EXECUTE",
        bits::USAGE => "USAGE",
        bits::CREATE => "CREATE",
        bits::CREATE_TEMP => "TEMP",
        bits::CONNECT => "CONNECT",
        bits::SET => "SET",
        bits::ALTER_SYSTEM => "ALTER SYSTEM",
        bits::MAINTAIN => "MAINTAIN",
        _ => "???",
    }
}

/// ExecuteGrantStmt / ExecAlterDefaultPrivilegesStmt: the privileges an
/// object type has, and the word the error names it by.
fn object_privileges(objtype: ObjectType, default_privileges: bool) -> Option<(u32, &'static str)> {
    use ObjectType as O;
    Some(match objtype {
        // GRANT ... ON TABLE may name a sequence.
        O::ObjectTable if default_privileges => (bits::RELATION, "relation"),
        O::ObjectTable => (bits::RELATION | bits::SEQUENCE, "relation"),
        O::ObjectSequence => (bits::SEQUENCE, "sequence"),
        O::ObjectDatabase => (bits::DATABASE, "database"),
        O::ObjectDomain => (bits::TYPE, "domain"),
        O::ObjectFunction => (bits::FUNCTION, "function"),
        O::ObjectLanguage => (bits::LANGUAGE, "language"),
        O::ObjectLargeobject => (bits::LARGEOBJECT, "large object"),
        O::ObjectSchema => (bits::SCHEMA, "schema"),
        O::ObjectProcedure => (bits::FUNCTION, "procedure"),
        O::ObjectRoutine => (bits::FUNCTION, "routine"),
        O::ObjectTablespace => (bits::TABLESPACE, "tablespace"),
        O::ObjectType => (bits::TYPE, "type"),
        O::ObjectFdw => (bits::FDW, "foreign-data wrapper"),
        O::ObjectForeignServer => (bits::FOREIGN_SERVER, "foreign server"),
        O::ObjectParameterAcl => (bits::PARAMETER_ACL, "parameter"),
        _ => return None,
    })
}

/// The privileges a GRANT names, with those of each column list:
/// `(relation-level privileges, [(column privileges, columns)])` — `None`
/// for ALL [PRIVILEGES].
struct Privileges<'a> {
    all: bool,
    mask: u32,
    columns: Vec<(Option<u32>, Vec<&'a str>)>,
}

/// ExecuteGrantStmt's privilege checks against the object type.
fn check_privileges<'a>(
    objtype: ObjectType,
    privileges: &'a [typedpg_pg_query::protobuf::Node],
    default_privileges: bool,
) -> Result<Privileges<'a>, DdlError> {
    let mut out = Privileges {
        all: privileges.is_empty(),
        mask: 0,
        columns: Vec::new(),
    };
    let Some((allowed, what)) = object_privileges(objtype, default_privileges) else {
        return Ok(out);
    };
    for p in privileges {
        let Some(node::Node::AccessPriv(ap)) = p.node.as_ref() else {
            continue;
        };
        if !ap.cols.is_empty() {
            if default_privileges {
                return Err(DdlError::Parse(
                    "default privileges cannot be set for columns".into(),
                ));
            }
            if objtype != ObjectType::ObjectTable {
                return Err(DdlError::Parse(
                    "column privileges are only valid for relations".into(),
                ));
            }
            let mask = if ap.priv_name.is_empty() {
                None
            } else {
                Some(privilege(&ap.priv_name)?)
            };
            out.columns
                .push((mask, ap.cols.iter().filter_map(node_string).collect()));
            continue;
        }
        if ap.priv_name.is_empty() {
            continue;
        }
        let bit = privilege(&ap.priv_name)?;
        if bit & !allowed != 0 {
            return Err(DdlError::Parse(format!(
                "invalid privilege type {} for {what}",
                privilege_name(bit)
            )));
        }
        out.mask |= bit;
    }
    Ok(out)
}

/// get_rolespec_oid for the grantees; `true` when PUBLIC is among them.
fn check_grantees(grantees: &[typedpg_pg_query::protobuf::Node]) -> Result<bool, DdlError> {
    let mut public = false;
    for g in grantees {
        let Some(node::Node::RoleSpec(rs)) = g.node.as_ref() else {
            continue;
        };
        match RoleSpecType::try_from(rs.roletype) {
            Ok(RoleSpecType::RolespecPublic) => public = true,
            Ok(RoleSpecType::RolespecCstring) if !super::session::role_may_exist(&rs.rolename) => {
                return Err(DdlError::TypeNotFound(format!(
                    "role \"{}\" does not exist",
                    rs.rolename
                )));
            }
            _ => {}
        }
    }
    Ok(public)
}

/// merge_acl_with_grant: WITH GRANT OPTION needs roles.
fn check_grant_option(stmt: &GrantStmt, to_public: bool) -> Result<(), DdlError> {
    if stmt.is_grant && stmt.grant_option && to_public {
        return Err(DdlError::Parse(
            "grant options can only be granted to roles".into(),
        ));
    }
    Ok(())
}

pub fn grant(interp: &PgCatalog, stmt: &GrantStmt) -> Result<(), DdlError> {
    let targtype = GrantTargetType::try_from(stmt.targtype).unwrap_or(GrantTargetType::Undefined);
    let objtype = ObjectType::try_from(stmt.objtype).unwrap_or(ObjectType::Undefined);
    // The target objects (objectNamesToOids / objectsInSchemaToOids).
    let mut targets = Vec::new();
    match targtype {
        GrantTargetType::AclTargetAllInSchema => {
            for obj in &stmt.objects {
                let Some(schema) = node_string(obj) else {
                    continue;
                };
                let Some(ns) = interp.namespace_oid(schema) else {
                    return Err(DdlError::TableNotFound(format!(
                        "schema \"{schema}\" does not exist"
                    )));
                };
                targets.extend(
                    relations_in_schema(interp, ns, objtype)
                        .into_iter()
                        .map(Target::Relation),
                );
            }
        }
        GrantTargetType::AclTargetObject => {
            for obj in &stmt.objects {
                if let Some(target) = resolve_target(interp, objtype, obj, stmt.is_grant)? {
                    targets.push(target);
                }
            }
        }
        _ => return Ok(()),
    }
    let to_public = check_grantees(&stmt.grantees)?;
    let privileges = check_privileges(objtype, &stmt.privileges, false)?;
    for target in targets {
        check_target(interp, objtype, &target, &privileges)?;
        check_grant_option(stmt, to_public)?;
    }
    Ok(())
}

/// A resolved GRANT target, as far as ExecGrant_* checks it.
enum Target {
    Relation(PgClassOid),
    Type(crate::oid::PgTypeOid),
    Language(String),
    Other,
}

/// getRelationsInNamespace for ALL TABLES / SEQUENCES IN SCHEMA.
fn relations_in_schema(
    interp: &PgCatalog,
    ns: crate::oid::PgNamespaceOid,
    objtype: ObjectType,
) -> Vec<PgClassOid> {
    let mut out: Vec<PgClassOid> = interp
        .pg_class
        .values()
        .filter(|c| c.relnamespace == ns)
        .filter(|c| match objtype {
            ObjectType::ObjectTable => matches!(
                c.relkind,
                RelKind::Table
                    | RelKind::View
                    | RelKind::MaterializedView
                    | RelKind::ForeignTable
                    | RelKind::Partitioned
            ),
            ObjectType::ObjectSequence => c.relkind == RelKind::Sequence,
            _ => false,
        })
        .map(|c| c.oid)
        .collect();
    out.sort();
    out
}

/// objectNamesToOids / get_object_address for one GRANT target.
fn resolve_target(
    interp: &PgCatalog,
    objtype: ObjectType,
    obj: &typedpg_pg_query::protobuf::Node,
    is_grant: bool,
) -> Result<Option<Target>, DdlError> {
    use ObjectType as O;
    let missing = |what: &str, name: &str| {
        Err(DdlError::TypeNotFound(format!(
            "{what} \"{name}\" does not exist"
        )))
    };
    Ok(Some(match (objtype, obj.node.as_ref()) {
        (O::ObjectTable | O::ObjectSequence, Some(node::Node::RangeVar(rv))) => {
            Target::Relation(super::util::lookup_relation(interp, rv)?.1)
        }
        (
            O::ObjectFunction | O::ObjectProcedure | O::ObjectRoutine,
            Some(inner @ node::Node::ObjectWithArgs(_)),
        ) => {
            super::comment::resolve_object(interp, objtype, inner)?;
            Target::Other
        }
        (O::ObjectSchema, _) => {
            if let Some(schema) = node_string(obj)
                && interp.namespace_oid(schema).is_none()
            {
                return Err(DdlError::TableNotFound(format!(
                    "schema \"{schema}\" does not exist"
                )));
            }
            Target::Other
        }
        (O::ObjectType | O::ObjectDomain, Some(node::Node::List(l))) => {
            let tn = typedpg_pg_query::protobuf::TypeName {
                names: l.items.clone(),
                ..Default::default()
            };
            let typ = super::util::lookup_type_name(&tn, interp)?;
            // get_object_address_type: DOMAIN names a domain.
            if objtype == O::ObjectDomain
                && interp.get_type(typ).map(|t| t.typtype)
                    != Some(crate::pg_catalog::TypType::Domain)
            {
                return Err(DdlError::Parse(format!(
                    "\"{}\" is not a domain",
                    super::util::format_type_for_message(interp, typ)
                )));
            }
            Target::Type(typ)
        }
        (O::ObjectLanguage, _) => {
            let Some(name) = node_string(obj) else {
                return Ok(None);
            };
            if !super::languages::exists(interp, name) {
                return missing("language", name);
            }
            Target::Language(name.to_owned())
        }
        (O::ObjectFdw, _) => {
            if let Some(name) = node_string(obj) {
                super::fdw::check_fdw(interp, name)?;
            }
            Target::Other
        }
        (O::ObjectForeignServer, _) => {
            if let Some(name) = node_string(obj) {
                super::fdw::check_server(interp, name)?;
            }
            Target::Other
        }
        (O::ObjectTablespace, _) => {
            if let Some(name) = node_string(obj)
                && !super::cluster::tablespace_exists(interp, name)
            {
                return missing("tablespace", name);
            }
            Target::Other
        }
        (O::ObjectLargeobject, Some(n)) => {
            let oid = match n {
                node::Node::Integer(i) => u32::try_from(i.ival).ok(),
                node::Node::Float(f) => f.fval.parse().ok(),
                _ => None,
            };
            if let Some(oid) = oid
                && !super::cluster::large_object_exists(interp, oid)
            {
                return Err(DdlError::TypeNotFound(format!(
                    "large object {oid} does not exist"
                )));
            }
            Target::Other
        }
        // ParameterAclCreate: a GRANT names a known parameter or a custom
        // `prefix.name` one; a REVOKE of an unknown one does nothing.
        (O::ObjectParameterAcl, _) => {
            if let Some(name) = node_string(obj)
                && is_grant
                && !super::guc::exists(interp, name)
            {
                return Err(DdlError::TypeNotFound(format!(
                    "unrecognized configuration parameter \"{name}\""
                )));
            }
            Target::Other
        }
        _ => Target::Other,
    }))
}

/// ExecGrant_Relation / ExecGrant_Type_check / ExecGrant_Language_check.
fn check_target(
    interp: &PgCatalog,
    objtype: ObjectType,
    target: &Target,
    privileges: &Privileges<'_>,
) -> Result<(), DdlError> {
    match target {
        Target::Relation(relid) => {
            let Some(class) = interp.pg_class.get(relid) else {
                return Ok(());
            };
            let relname = &class.relname;
            match class.relkind {
                RelKind::Index | RelKind::PartitionedIndex => {
                    return Err(DdlError::Parse(format!("\"{relname}\" is an index")));
                }
                RelKind::CompositeType => {
                    return Err(DdlError::Parse(format!(
                        "\"{relname}\" is a composite type"
                    )));
                }
                _ => {}
            }
            let sequence = class.relkind == RelKind::Sequence;
            if objtype == ObjectType::ObjectSequence && !sequence {
                return Err(DdlError::Parse(format!("\"{relname}\" is not a sequence")));
            }
            // GRANT ... ON TABLE: USAGE is a sequence's only; on a
            // sequence the others only warn.
            if objtype == ObjectType::ObjectTable
                && !sequence
                && !privileges.all
                && privileges.mask & !bits::RELATION != 0
            {
                return Err(DdlError::Parse(
                    "invalid privilege type USAGE for table".into(),
                ));
            }
            for (mask, columns) in &privileges.columns {
                let mask = mask.unwrap_or(bits::COLUMN);
                if mask & !bits::COLUMN != 0 {
                    return Err(DdlError::Parse(format!(
                        "invalid privilege type {} for column",
                        privilege_name(mask)
                    )));
                }
                // expand_col_privileges: get_attnum, system columns
                // included.
                for column in columns {
                    let system = crate::pg_catalog::SYSTEM_COLUMNS
                        .iter()
                        .any(|(n, ..)| n == column);
                    if interp.attribute_by_name(*relid, column).is_none() && !system {
                        return Err(DdlError::Parse(format!(
                            "column \"{column}\" of relation \"{relname}\" does not exist"
                        )));
                    }
                }
            }
        }
        Target::Type(typ) => {
            let Some(t) = interp.get_type(*typ) else {
                return Ok(());
            };
            // IsTrueArrayType.
            if t.typcategory == crate::pg_catalog::TypCategory::Array
                && t.typelem.is_some()
                && t.typelem.and_then(|e| interp.array_type_of(e)) == Some(t.oid)
            {
                return Err(DdlError::Parse(
                    "cannot set privileges of array types (Set the privileges of the element \
                     type instead.)"
                        .into(),
                ));
            }
            if t.typtype == crate::pg_catalog::TypType::Multirange {
                return Err(DdlError::Parse(
                    "cannot set privileges of multirange types (Set the privileges of the range \
                     type instead.)"
                        .into(),
                ));
            }
        }
        // lanpltrusted: C and internal functions are superuser-only.
        Target::Language(name) if matches!(name.as_str(), "c" | "internal") => {
            return Err(DdlError::Parse(format!(
                "language \"{name}\" is not trusted (GRANT and REVOKE are not allowed on \
                 untrusted languages, because only superusers can use untrusted languages.)"
            )));
        }
        _ => {}
    }
    Ok(())
}

/// ALTER DEFAULT PRIVILEGES [FOR ROLE ...] [IN SCHEMA ...] GRANT / REVOKE
/// (ExecAlterDefaultPrivilegesStmt, SetDefaultACLsInSchemas, SetDefaultACL).
pub fn alter_default_privileges(
    interp: &PgCatalog,
    stmt: &typedpg_pg_query::protobuf::AlterDefaultPrivilegesStmt,
) -> Result<(), DdlError> {
    let mut schemas: Vec<&str> = Vec::new();
    for opt in &stmt.options {
        let Some(node::Node::DefElem(de)) = opt.node.as_ref() else {
            continue;
        };
        if de.defname != "schemas" {
            continue;
        }
        let Some(node::Node::List(list)) = de.arg.as_deref().and_then(|a| a.node.as_ref()) else {
            continue;
        };
        schemas.extend(list.items.iter().filter_map(node_string));
    }
    let Some(action) = stmt.action.as_ref() else {
        return Ok(());
    };
    let objtype = ObjectType::try_from(action.objtype).unwrap_or(ObjectType::Undefined);
    let to_public = check_grantees(&action.grantees)?;
    check_privileges(objtype, &action.privileges, true)?;
    for schema in &schemas {
        if interp.namespace_oid(schema).is_none() {
            return Err(DdlError::TableNotFound(format!(
                "schema \"{schema}\" does not exist"
            )));
        }
    }
    let targets = if schemas.is_empty() { 1 } else { schemas.len() };
    for _ in 0..targets {
        let object_word = match objtype {
            ObjectType::ObjectSchema => Some("GRANT/REVOKE ON SCHEMAS"),
            ObjectType::ObjectLargeobject => Some("GRANT/REVOKE ON LARGE OBJECTS"),
            _ => None,
        };
        if let Some(word) = object_word
            && !schemas.is_empty()
        {
            return Err(DdlError::Parse(format!(
                "cannot use IN SCHEMA clause when using {word}"
            )));
        }
        check_grant_option(action, to_public)?;
    }
    Ok(())
}
