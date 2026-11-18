//! Object privileges: `GRANT` / `REVOKE`, `ALTER DEFAULT PRIVILEGES`, the
//! object ACLs and owners (`pg_class.relacl`, `pg_attribute.attacl`,
//! `pg_proc.proacl`, `pg_namespace.nspacl`, `pg_type.typacl`,
//! `pg_language.lanacl` and `pg_default_acl`), and the `has_*_privilege`
//! queries of PostgreSQL's `acl.c` / `aclchk.c`.
//!
//! The model follows PG's own arithmetic:
//!
//! - an object's ACL is a list of [`AclItem`]s, each a `(grantee, grantor)` pair
//!   with privilege and grant-option bits; `NULL` (absent here) means
//!   `acldefault`, the owner's entry plus PUBLIC's;
//! - a GRANT merges the privileges into the entry for
//!   `(grantee, select_best_grantor)`, a REVOKE removes them from it
//!   (`merge_acl_with_grant` / `aclupdate`), and losing grant options cascades
//!   into the grants that grantee made (`recursive_revoke`);
//! - privileges are read by `aclmask`: entries for the role, for PUBLIC, and
//!   for every role it `has_privs_of_role` (inheritable memberships), with a
//!   superuser bypass and the predefined `pg_read_all_data` /
//!   `pg_write_all_data` / `pg_maintain` special cases.

use std::collections::HashMap;

use typedpg_pg_query::ast::{
    self, DropBehavior, GrantStmt, GrantTargetType, ObjectType, RoleSpecType,
};

use super::DdlError;
use super::roles::{RoleId, current_role, resolve_role_spec};
use super::util::node_string;
use crate::oid::{PgClassOid, PgGenericOid, PgLanguageOid, PgNamespaceOid, PgProcOid, PgTypeOid};
use crate::pg_catalog::{AclCatalog, PgCatalog, RelKind, SeedAcl, TypType};

/// `ACL_*` privilege bits (acl.h / parsenodes.h).
pub(crate) mod bits {
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

/// A privilege the [`PgCatalog::has_table_privilege`] family checks — PG's
/// `ACL_*` names. One enum serves every object kind; a privilege an object
/// kind doesn't have is simply never held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Privilege {
    Select,
    Insert,
    Update,
    Delete,
    Truncate,
    References,
    Trigger,
    Execute,
    Usage,
    Create,
    Maintain,
}

impl Privilege {
    pub(crate) fn bit(self) -> u32 {
        match self {
            Privilege::Select => bits::SELECT,
            Privilege::Insert => bits::INSERT,
            Privilege::Update => bits::UPDATE,
            Privilege::Delete => bits::DELETE,
            Privilege::Truncate => bits::TRUNCATE,
            Privilege::References => bits::REFERENCES,
            Privilege::Trigger => bits::TRIGGER,
            Privilege::Execute => bits::EXECUTE,
            Privilege::Usage => bits::USAGE,
            Privilege::Create => bits::CREATE,
            Privilege::Maintain => bits::MAINTAIN,
        }
    }

    /// The privilege's SQL name, as `GRANT` spells it.
    pub fn name(self) -> &'static str {
        match self {
            Privilege::Select => "SELECT",
            Privilege::Insert => "INSERT",
            Privilege::Update => "UPDATE",
            Privilege::Delete => "DELETE",
            Privilege::Truncate => "TRUNCATE",
            Privilege::References => "REFERENCES",
            Privilege::Trigger => "TRIGGER",
            Privilege::Execute => "EXECUTE",
            Privilege::Usage => "USAGE",
            Privilege::Create => "CREATE",
            Privilege::Maintain => "MAINTAIN",
        }
    }
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

// ─── ACL state ──────────────────────────────────────────────────────────────

/// An object that carries an ACL (`aclitem[]`), identified the way PG's ACL
/// functions take it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum AclObject {
    Relation(PgClassOid),
    Function(PgProcOid),
    Schema(PgNamespaceOid),
    Type(PgTypeOid),
    Language(PgLanguageOid),
}

impl AclObject {
    fn oid(self) -> PgGenericOid {
        PgGenericOid::from_nonzero(match self {
            AclObject::Relation(oid) => oid.into_nonzero(),
            AclObject::Function(oid) => oid.into_nonzero(),
            AclObject::Schema(oid) => oid.into_nonzero(),
            AclObject::Type(oid) => oid.into_nonzero(),
            AclObject::Language(oid) => oid.into_nonzero(),
        })
    }

    /// The PG object kind whose `acldefault` / rights apply.
    pub(crate) fn kind(self, db: &PgCatalog) -> AclObjectKind {
        match self {
            AclObject::Relation(relid) => {
                if db.pg_class.get(&relid).map(|c| c.relkind) == Some(RelKind::Sequence) {
                    AclObjectKind::Sequence
                } else {
                    AclObjectKind::Relation
                }
            }
            AclObject::Function(_) => AclObjectKind::Function,
            AclObject::Schema(_) => AclObjectKind::Schema,
            AclObject::Type(_) => AclObjectKind::Type,
            AclObject::Language(_) => AclObjectKind::Language,
        }
    }
}

/// The kinds of object (and column) ACLs, as `acldefault` / the GRANT checks
/// distinguish them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum AclObjectKind {
    Relation,
    Sequence,
    Function,
    Schema,
    Type,
    Language,
}

impl AclObjectKind {
    fn all_rights(self) -> u32 {
        match self {
            AclObjectKind::Relation => bits::RELATION,
            AclObjectKind::Sequence => bits::SEQUENCE,
            AclObjectKind::Function => bits::FUNCTION,
            AclObjectKind::Schema => bits::SCHEMA,
            AclObjectKind::Type => bits::TYPE,
            AclObjectKind::Language => bits::LANGUAGE,
        }
    }

    /// `acldefault`: `(world default, owner default)`. The owner's entry
    /// carries no grant options (his come from being the owner).
    fn defaults(self) -> (u32, u32) {
        match self {
            AclObjectKind::Relation => (0, bits::RELATION),
            AclObjectKind::Sequence => (0, bits::SEQUENCE),
            AclObjectKind::Function => (bits::EXECUTE, bits::FUNCTION),
            AclObjectKind::Schema => (0, bits::SCHEMA),
            AclObjectKind::Type => (bits::USAGE, bits::TYPE),
            AclObjectKind::Language => (bits::USAGE, bits::LANGUAGE),
        }
    }
}

/// The object kinds `ALTER DEFAULT PRIVILEGES` has (`defaclobjtype`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum DefaultAclClass {
    Table,
    Sequence,
    Function,
    Type,
    Schema,
}

impl DefaultAclClass {
    fn of(objtype: ObjectType) -> Option<Self> {
        Some(match objtype {
            ObjectType::ObjectTable => DefaultAclClass::Table,
            ObjectType::ObjectSequence => DefaultAclClass::Sequence,
            ObjectType::ObjectFunction
            | ObjectType::ObjectProcedure
            | ObjectType::ObjectRoutine => DefaultAclClass::Function,
            ObjectType::ObjectType | ObjectType::ObjectDomain => DefaultAclClass::Type,
            ObjectType::ObjectSchema => DefaultAclClass::Schema,
            _ => return None,
        })
    }

    fn kind(self) -> AclObjectKind {
        match self {
            DefaultAclClass::Table => AclObjectKind::Relation,
            DefaultAclClass::Sequence => AclObjectKind::Sequence,
            DefaultAclClass::Function => AclObjectKind::Function,
            DefaultAclClass::Type => AclObjectKind::Type,
            DefaultAclClass::Schema => AclObjectKind::Schema,
        }
    }
}

/// An ACL's grantee: `PUBLIC` (`ACL_ID_PUBLIC`, grantee 0) or one role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Grantee {
    Public,
    Role(RoleId),
}

/// One `AclItem`: who may do what, granted by whom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AclItem {
    pub(crate) grantee: Grantee,
    pub(crate) grantor: RoleId,
    pub(crate) privileges: u32,
    pub(crate) grant_options: u32,
}

impl AclItem {
    fn same_entry(&self, other: &AclItem) -> bool {
        self.grantee == other.grantee && self.grantor == other.grantor
    }
}

/// `acldefault`.
fn acldefault(kind: AclObjectKind, owner: RoleId) -> Vec<AclItem> {
    let (world, own) = kind.defaults();
    let mut acl = Vec::new();
    if world != 0 {
        acl.push(AclItem {
            grantee: Grantee::Public,
            grantor: owner,
            privileges: world,
            grant_options: 0,
        });
    }
    if own != 0 {
        acl.push(AclItem {
            grantee: Grantee::Role(owner),
            grantor: owner,
            privileges: own,
            grant_options: 0,
        });
    }
    acl
}

/// `aclnewowner`: substitute `old` by `new` wherever the ACL mentions it as
/// grantor or grantee, merging entries that become identical.
fn aclnewowner(acl: &[AclItem], old: RoleId, new: RoleId) -> Vec<AclItem> {
    let mut out: Vec<AclItem> = Vec::new();
    for item in acl {
        let mut item = *item;
        if item.grantor == old {
            item.grantor = new;
        }
        if item.grantee == Grantee::Role(old) {
            item.grantee = Grantee::Role(new);
        }
        if let Some(existing) = out.iter_mut().find(|e| e.same_entry(&item)) {
            existing.privileges |= item.privileges;
            existing.grant_options |= item.grant_options;
        } else {
            out.push(item);
        }
    }
    out
}

/// Compare two ACLs as sets (`aclequal` after `aclitemsort`).
fn acl_equal(left: &[AclItem], right: &[AclItem]) -> bool {
    let normalize = |acl: &[AclItem]| {
        let mut rows: Vec<(Grantee, RoleId, u32, u32)> = acl
            .iter()
            .map(|i| (i.grantee, i.grantor, i.privileges, i.grant_options))
            .collect();
        rows.sort();
        rows
    };
    normalize(left) == normalize(right)
}

/// `DropBehavior` as `aclupdate`'s `behavior`.
fn is_cascade(behavior: DropBehavior) -> bool {
    behavior == DropBehavior::DropCascade
}

/// `aclupdate`: add or remove privileges in the entry for
/// `(grantee, grantor)`. Losing grant options revokes the grants that
/// grantee made (`recursive_revoke`), cascading when `behavior` says so.
fn acl_update(
    db: &PgCatalog,
    acl: &[AclItem],
    modification: &AclItem,
    add: bool,
    owner: RoleId,
    behavior: DropBehavior,
) -> Result<Vec<AclItem>, DdlError> {
    let mut new_acl = acl.to_vec();
    let at = new_acl.iter().position(|i| i.same_entry(modification));
    let at = match at {
        Some(at) => at,
        None => {
            new_acl.push(AclItem {
                grantee: modification.grantee,
                grantor: modification.grantor,
                privileges: 0,
                grant_options: 0,
            });
            new_acl.len() - 1
        }
    };
    let old_goptions = new_acl[at].grant_options;
    if add {
        new_acl[at].privileges |= modification.privileges;
        new_acl[at].grant_options |= modification.grant_options;
    } else {
        new_acl[at].privileges &= !modification.privileges;
        new_acl[at].grant_options &= !modification.grant_options;
    }
    let new_goptions = new_acl[at].grant_options;
    if new_acl[at].privileges == 0 && new_goptions == 0 {
        new_acl.remove(at);
    }
    let lost = old_goptions & !new_goptions;
    if lost != 0 {
        new_acl = recursive_revoke(db, new_acl, modification.grantee, lost, owner, behavior)?;
    }
    Ok(new_acl)
}

/// `recursive_revoke`: drop `revoke_privileges` from every entry the
/// `grantee` granted, refusing (RESTRICT) when anything would be left behind.
fn recursive_revoke(
    db: &PgCatalog,
    mut acl: Vec<AclItem>,
    grantee: Grantee,
    revoke_privileges: u32,
    owner: RoleId,
    behavior: DropBehavior,
) -> Result<Vec<AclItem>, DdlError> {
    let Grantee::Role(grantee_role) = grantee else {
        // PUBLIC can't hold grant options.
        return Ok(acl);
    };
    if grantee_role == owner {
        return Ok(acl);
    }
    loop {
        // The grantee may still have the grant options through another
        // grantor (or as a superuser).
        let still = grant_options_held(db, grantee_role, owner, &acl, revoke_privileges);
        let revoke = revoke_privileges & !still;
        if revoke == 0 {
            return Ok(acl);
        }
        let next = acl
            .iter()
            .find(|i| i.grantor == grantee_role && (i.privileges & revoke) != 0);
        let Some(item) = next else {
            return Ok(acl);
        };
        if !is_cascade(behavior) {
            return Err(DdlError::DependencyError(
                "dependent privileges exist".into(),
            ));
        }
        let item = *item;
        let mod_item = AclItem {
            grantee: item.grantee,
            grantor: grantee_role,
            privileges: revoke,
            grant_options: revoke,
        };
        acl = acl_update(db, &acl, &mod_item, false, owner, behavior)?;
    }
}

/// The grant options `role` holds over `bits` in `acl` (PG's `aclmask` of
/// `ACL_GRANT_OPTION_FOR(bits)`): the owner and a superuser implicitly hold
/// every option.
fn grant_options_held(
    db: &PgCatalog,
    role: RoleId,
    owner: RoleId,
    acl: &[AclItem],
    bits: u32,
) -> u32 {
    if role == owner || db.roles.is_superuser(role) {
        return bits;
    }
    let mut result = 0;
    for item in acl {
        let matches = match item.grantee {
            Grantee::Public => true,
            Grantee::Role(grantee) => grantee == role || db.roles.has_privs_of_role(role, grantee),
        };
        if matches {
            result |= item.grant_options & bits;
        }
    }
    result
}

/// `aclmask` of plain privileges (`pg_class_aclmask` & co.'s core): the ACL
/// entries for `role`, for PUBLIC, and for every role it has the privileges
/// of.
fn aclmask_privileges(
    db: &PgCatalog,
    role: RoleId,
    owner: RoleId,
    acl: &[AclItem],
    mask: u32,
) -> u32 {
    if mask == 0 {
        return 0;
    }
    let mut result = 0;
    for item in acl {
        if matches!(item.grantee, Grantee::Public) || item.grantee == Grantee::Role(role) {
            result |= item.privileges & mask;
        }
    }
    let remaining = mask & !result;
    if remaining != 0 {
        for item in acl {
            let Grantee::Role(grantee) = item.grantee else {
                continue;
            };
            if grantee == role {
                continue;
            }
            if (item.privileges & remaining) != 0 && db.roles.has_privs_of_role(role, grantee) {
                result |= item.privileges & mask;
            }
        }
    }
    let _ = owner;
    result
}

/// The catalog's ACLs, owners and default ACLs. Part of the transactional
/// catalog (`Shared` keeps clones cheap, rollback restores the snapshot).
#[derive(Debug, Clone, Default)]
pub(crate) struct Acls {
    /// The owner of every object the migrations created (or whose owner a
    /// migration changed). Absent objects are owned by the seed's bootstrap
    /// superuser, or by the migrations' user when created before a hook ran.
    owners: HashMap<AclObject, RoleId>,
    /// Explicit ACLs, by object. Absent means `acldefault`.
    items: HashMap<AclObject, Vec<AclItem>>,
    /// `pg_attribute.attacl`, by `(attrelid, attnum)`. Absent means NULL.
    columns: HashMap<(PgClassOid, i16), Vec<AclItem>>,
    /// `pg_default_acl`: `(defaclrole, defaclnamespace, defaclobjtype)`.
    defaults: HashMap<(RoleId, Option<PgNamespaceOid>, DefaultAclClass), Vec<AclItem>>,
}

impl Acls {
    pub(crate) fn forget_object(&mut self, object: AclObject) {
        self.owners.remove(&object);
        self.items.remove(&object);
    }

    pub(crate) fn forget_columns_of(&mut self, relid: PgClassOid) {
        self.columns.retain(|(rel, _), _| *rel != relid);
    }

    /// A dropped schema takes its `ALTER DEFAULT PRIVILEGES IN SCHEMA`
    /// entries with it (`pg_default_acl` depends on the namespace).
    pub(crate) fn forget_schema_defaults(&mut self, schema: PgNamespaceOid) {
        self.defaults.retain(|(_, ns, _), _| *ns != Some(schema));
    }

    pub(crate) fn forget_column(&mut self, relid: PgClassOid, attnum: i16) {
        self.columns.remove(&(relid, attnum));
    }

    pub(crate) fn forget_role(&mut self, role: RoleId) {
        self.defaults.retain(|(owner, ..), _| *owner != role);
    }

    /// Whether `role` owns anything (an object, or default privileges).
    pub(crate) fn owns_anything(&self, role: RoleId) -> bool {
        self.owners.values().any(|&o| o == role) || self.defaults.keys().any(|k| k.0 == role)
    }

    /// Whether a stored ACL mentions `role` as grantee or grantor, or as the
    /// owner of default privileges (`pg_shdepend`'s ACL dependencies).
    pub(crate) fn mentions_role(&self, role: RoleId) -> bool {
        let owner_of = |object: AclObject| self.owners.get(&object).copied();
        for (object, acl) in &self.items {
            let owner = owner_of(*object);
            if acl.iter().any(|item| {
                (item.grantee == Grantee::Role(role) || item.grantor == role) && owner != Some(role)
            }) {
                return true;
            }
        }
        for ((relid, _), acl) in &self.columns {
            let owner = owner_of(AclObject::Relation(*relid));
            if acl.iter().any(|item| {
                (item.grantee == Grantee::Role(role) || item.grantor == role) && owner != Some(role)
            }) {
                return true;
            }
        }
        self.defaults.iter().any(|(key, acl)| {
            key.0 == role
                || acl.iter().any(|item| {
                    (item.grantee == Grantee::Role(role) || item.grantor == role)
                        && item.grantor != key.0
                })
        })
    }

    /// `REASSIGN OWNED` of `old` to `new`: every object `old` owns changes
    /// owner, with `aclnewowner` rewriting its ACL's mentions.
    pub(crate) fn reassign_owner(&mut self, old: RoleId, new: RoleId) {
        if old == new {
            return;
        }
        let mut objects: Vec<AclObject> = self
            .owners
            .iter()
            .filter(|(_, owner)| **owner == old)
            .map(|(&object, _)| object)
            .collect();
        objects.sort();
        for object in objects {
            self.owners.insert(object, new);
            if let Some(acl) = self.items.get_mut(&object) {
                *acl = aclnewowner(acl, old, new);
            }
            if let AclObject::Relation(relid) = object {
                for ((rel, _), acl) in self.columns.iter_mut() {
                    if *rel == relid {
                        *acl = aclnewowner(acl, old, new);
                    }
                }
            }
        }
    }

    /// Whether the object's owner has already been recorded.
    pub(crate) fn has_owner(&self, object: AclObject) -> bool {
        self.owners.contains_key(&object)
    }

    /// The ACL stored for `object`, if any.
    pub(crate) fn explicit(&self, object: AclObject) -> Option<&Vec<AclItem>> {
        self.items.get(&object)
    }

    /// Restore an object's stored ACL (a `CREATE OR REPLACE` keeps it).
    pub(crate) fn set_explicit(&mut self, object: AclObject, acl: Vec<AclItem>) {
        self.items.insert(object, acl);
    }

    pub(crate) fn explicit_column(&self, relid: PgClassOid, attnum: i16) -> Option<&Vec<AclItem>> {
        self.columns.get(&(relid, attnum))
    }

    /// `get_user_default_acl`: the ACL a newly created object of `class` in
    /// `schema`, owned by `owner`, starts with. `None` when neither a global
    /// nor a per-schema entry exists (the object keeps a NULL ACL).
    pub(crate) fn default_acl(
        &self,
        owner: RoleId,
        schema: Option<PgNamespaceOid>,
        class: DefaultAclClass,
    ) -> Option<Vec<AclItem>> {
        let global = self.defaults.get(&(owner, None, class));
        let per_schema = schema.and_then(|ns| self.defaults.get(&(owner, Some(ns), class)));
        if global.is_none() && per_schema.is_none() {
            return None;
        }
        let mut acl = global
            .cloned()
            .unwrap_or_else(|| acldefault(class.kind(), owner));
        if let Some(per_schema) = per_schema {
            for item in per_schema {
                let modification = AclItem {
                    grantee: item.grantee,
                    grantor: item.grantor,
                    privileges: item.privileges,
                    grant_options: item.grant_options,
                };
                // `aclmerge`: adds each entry (RESTRICT never matters when
                // adding).
                acl = acl_update_unchecked(&acl, &modification);
            }
        }
        // `get_user_default_acl` returns NULL when the merged ACL equals the
        // hard-wired default, so the object keeps a NULL ACL.
        if acl_equal(&acl, &acldefault(class.kind(), owner)) {
            None
        } else {
            Some(acl)
        }
    }

    /// `SetDefaultACL`: store the new default ACL, or remove the entry when
    /// it equals the hard-wired default.
    fn set_default_acl(
        &mut self,
        owner: RoleId,
        schema: Option<PgNamespaceOid>,
        class: DefaultAclClass,
        acl: Vec<AclItem>,
    ) {
        let default = if schema.is_none() {
            acldefault(class.kind(), owner)
        } else {
            Vec::new()
        };
        if acl_equal(&acl, &default) {
            self.defaults.remove(&(owner, schema, class));
        } else {
            self.defaults.insert((owner, schema, class), acl);
        }
    }
}

/// `aclmerge`'s inner `aclupdate` with `ACL_MODECHG_ADD`: it never loses
/// grant options, so it can't fail.
fn acl_update_unchecked(acl: &[AclItem], modification: &AclItem) -> Vec<AclItem> {
    let mut new_acl = acl.to_vec();
    let at = new_acl.iter().position(|i| i.same_entry(modification));
    let at = match at {
        Some(at) => at,
        None => {
            new_acl.push(AclItem {
                grantee: modification.grantee,
                grantor: modification.grantor,
                privileges: 0,
                grant_options: 0,
            });
            new_acl.len() - 1
        }
    };
    new_acl[at].privileges |= modification.privileges;
    new_acl[at].grant_options |= modification.grant_options;
    new_acl
}

// ─── Owners and creation hooks ──────────────────────────────────────────────

impl PgCatalog {
    /// The owner of an ACL-carrying object: recorded when the migrations
    /// created (or re-owned) it; else the bootstrap superuser for the seed's
    /// built-ins, and the migrations' user for anything else.
    pub(crate) fn owner_of(&self, object: AclObject) -> RoleId {
        if let Some(&owner) = self.acls.owners.get(&object) {
            return owner;
        }
        if object.oid().get() < self.seed_oid_end {
            self.roles.bootstrap
        } else {
            self.roles.migrations_user
        }
    }

    /// The ACL in effect for `object`: its own, or `acldefault`.
    pub(crate) fn acl_of(&self, object: AclObject) -> Vec<AclItem> {
        match self.acls.explicit(object) {
            Some(acl) => acl.clone(),
            None => acldefault(object.kind(self), self.owner_of(object)),
        }
    }

    /// The column's `attacl` (`None` = NULL).
    pub(crate) fn column_acl_of(&self, relid: PgClassOid, attnum: i16) -> Vec<AclItem> {
        self.acls
            .explicit_column(relid, attnum)
            .cloned()
            .unwrap_or_default()
    }

    /// Record a new object's owner (and, when the role has default
    /// privileges for its class, its initial ACL).
    pub(crate) fn set_object_owner(&mut self, object: AclObject, owner: RoleId) {
        self.acls.owners.insert(object, owner);
    }

    /// A newly created object: owned by `owner`, ACL from `owner`'s default
    /// privileges (`get_user_default_acl`). `schema` is where the object
    /// lives; `None` for a schema itself (whose default privileges are global
    /// only).
    pub(crate) fn object_created(
        &mut self,
        object: AclObject,
        owner: RoleId,
        schema: Option<PgNamespaceOid>,
        class: DefaultAclClass,
    ) {
        self.acls.owners.insert(object, owner);
        if let Some(acl) = self.acls.default_acl(owner, schema, class) {
            self.acls.items.insert(object, acl);
        }
    }

    /// A dependent object (a table's row type, an array type, an index):
    /// owned by its parent's owner, without default privileges of its own
    /// (`TypeCreate`'s `isDependentType`, `index_create`'s owner).
    pub(crate) fn dependent_object_created(&mut self, object: AclObject, owner: RoleId) {
        self.acls.owners.insert(object, owner);
    }

    /// `ALTER ... OWNER TO` of one object: `ATExecChangeOwner` /
    /// `AlterObjectOwnerInternal`, with `aclnewowner`.
    pub(crate) fn change_object_owner(&mut self, object: AclObject, new_owner: RoleId) {
        let old_owner = self.owner_of(object);
        if old_owner == new_owner {
            return;
        }
        self.set_object_owner(object, new_owner);
        if let Some(acl) = self.acls.items.get_mut(&object) {
            *acl = aclnewowner(acl, old_owner, new_owner);
        }
        if let AclObject::Relation(relid) = object {
            for ((rel, _), acl) in self.acls.columns.iter_mut() {
                if *rel == relid {
                    *acl = aclnewowner(acl, old_owner, new_owner);
                }
            }
        }
    }
}

/// `ALTER TABLE ... OWNER TO` of `relid`: the relation, its row type and
/// that type's array, its indexes, and the sequences it owns follow
/// (`ATExecChangeOwner`).
pub(crate) fn change_relation_owner(db: &mut PgCatalog, relid: PgClassOid, new_owner: RoleId) {
    let mut objects = vec![AclObject::Relation(relid)];
    if let Some(class) = db.pg_class.get(&relid)
        && let Some(row_type) = class.reltype
    {
        objects.push(AclObject::Type(row_type));
        if let Some(array) = db.array_type_of(row_type) {
            objects.push(AclObject::Type(array));
        }
    }
    let mut indexes: Vec<PgClassOid> = db
        .pg_index
        .values()
        .filter(|i| i.indrelid == relid)
        .map(|i| i.indexrelid)
        .collect();
    indexes.sort();
    objects.extend(indexes.into_iter().map(AclObject::Relation));
    let sequences = super::sequences::owned_sequences(db, relid, None);
    objects.extend(sequences.into_iter().map(AclObject::Relation));
    for object in objects {
        db.change_object_owner(object, new_owner);
    }
}

/// The owner a new relation gets: the current role (the migrations' user
/// without `SET ROLE`).
pub(crate) fn relation_created(
    db: &mut PgCatalog,
    relid: PgClassOid,
    nsoid: PgNamespaceOid,
    owner: RoleId,
    class: DefaultAclClass,
) {
    db.object_created(AclObject::Relation(relid), owner, Some(nsoid), class);
}

/// The owner a new index or TOAST table gets: its table's (`index_create`
/// copies `relowner` from the parent).
pub(crate) fn relation_created_dependent(db: &mut PgCatalog, relid: PgClassOid, owner: RoleId) {
    db.dependent_object_created(AclObject::Relation(relid), owner);
}

/// A new index: `index_create` gives it its table's owner.
pub(crate) fn index_created(db: &mut PgCatalog, index: PgClassOid, table: PgClassOid) {
    let owner = db.owner_of(AclObject::Relation(table));
    db.dependent_object_created(AclObject::Relation(index), owner);
}

/// A new function / procedure / aggregate (`ProcedureCreate`).
pub(crate) fn function_created(
    db: &mut PgCatalog,
    proc_oid: PgProcOid,
    nsoid: PgNamespaceOid,
    owner: RoleId,
) {
    db.object_created(
        AclObject::Function(proc_oid),
        owner,
        Some(nsoid),
        DefaultAclClass::Function,
    );
}

/// A new explicitly created type (base, enum, domain, range, composite).
pub(crate) fn type_created(
    db: &mut PgCatalog,
    type_oid: PgTypeOid,
    nsoid: PgNamespaceOid,
    owner: RoleId,
) {
    db.object_created(
        AclObject::Type(type_oid),
        owner,
        Some(nsoid),
        DefaultAclClass::Type,
    );
}

/// A dependent type (array, multirange, row type): no ACL of its own.
pub(crate) fn type_created_dependent(db: &mut PgCatalog, type_oid: PgTypeOid, owner: RoleId) {
    db.dependent_object_created(AclObject::Type(type_oid), owner);
}

/// A new schema (`NamespaceCreate`): owned by its `AUTHORIZATION` role, with
/// the schema default privileges (`isTemp` schemas have none).
pub(crate) fn schema_created(
    db: &mut PgCatalog,
    nsoid: PgNamespaceOid,
    owner: RoleId,
    is_temp: bool,
) {
    db.acls.items.remove(&AclObject::Schema(nsoid));
    db.acls.owners.insert(AclObject::Schema(nsoid), owner);
    if is_temp {
        return;
    }
    if let Some(acl) = db.acls.default_acl(owner, None, DefaultAclClass::Schema) {
        db.acls.items.insert(AclObject::Schema(nsoid), acl);
    }
}

/// A new language (`CreateLanguage`): owned by the current role. Languages
/// have no `ALTER DEFAULT PRIVILEGES` class; their ACL stays NULL
/// (`acldefault`'s PUBLIC USAGE).
pub(crate) fn language_created(db: &mut PgCatalog, lang: PgLanguageOid, owner: RoleId) {
    db.acls.owners.insert(AclObject::Language(lang), owner);
}

// ─── GRANT / REVOKE ─────────────────────────────────────────────────────────

/// The privileges a GRANT names, with those of each column list:
/// `(all, relation-level privileges, [(column privileges, columns)])` —
/// `None` for ALL [PRIVILEGES].
struct Privileges<'a> {
    all: bool,
    mask: u32,
    columns: Vec<(Option<u32>, Vec<&'a str>)>,
}

/// ExecuteGrantStmt's privilege checks against the object type.
fn check_privileges<'a>(
    objtype: ObjectType,
    privileges: &'a [Option<typedpg_pg_query::ast::Node>],
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
        let Some(ast::Node::AccessPriv(ap)) = p.as_ref() else {
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
            out.columns.push((
                mask,
                ap.cols
                    .iter()
                    .filter_map(|n| node_string(n.as_ref()))
                    .collect(),
            ));
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

/// Resolve the GRANT's grantees (`get_rolespec_oid`): PUBLIC or a role,
/// interned as external if the catalog didn't know it.
fn resolve_grantees(
    interp: &mut PgCatalog,
    grantees: &[Option<typedpg_pg_query::ast::Node>],
) -> Result<Vec<Grantee>, DdlError> {
    let mut out = Vec::new();
    for g in grantees {
        let Some(ast::Node::RoleSpec(rs)) = g.as_ref() else {
            continue;
        };
        match rs.roletype {
            RoleSpecType::RolespecPublic => out.push(Grantee::Public),
            RoleSpecType::RolespecCstring if !super::session::role_may_exist(&rs.rolename) => {
                return Err(DdlError::TypeNotFound(format!(
                    "role \"{}\" does not exist",
                    rs.rolename
                )));
            }
            _ => out.push(Grantee::Role(resolve_role_spec(interp, rs)?)),
        }
    }
    Ok(out)
}

/// merge_acl_with_grant: WITH GRANT OPTION needs roles.
fn check_grant_option(stmt: &GrantStmt, grantees: &[Grantee]) -> Result<(), DdlError> {
    if stmt.is_grant && stmt.grant_option && grantees.contains(&Grantee::Public) {
        return Err(DdlError::Parse(
            "grant options can only be granted to roles".into(),
        ));
    }
    Ok(())
}

/// A resolved GRANT target.
enum Target {
    Relation(PgClassOid),
    Function(PgProcOid),
    Schema(PgNamespaceOid),
    Type(PgTypeOid),
    Language(PgLanguageOid),
    Other,
}

pub fn grant(interp: &mut PgCatalog, stmt: &GrantStmt) -> Result<(), DdlError> {
    let targtype = stmt.targtype;
    let objtype = stmt.objtype;
    // The target objects (objectNamesToOids / objectsInSchemaToOids).
    let mut targets = Vec::new();
    match targtype {
        GrantTargetType::AclTargetAllInSchema => {
            for obj in &stmt.objects {
                let Some(schema) = node_string(obj.as_ref()) else {
                    continue;
                };
                let Some(ns) = interp.namespace_oid(schema) else {
                    return Err(DdlError::TableNotFound(format!(
                        "schema \"{schema}\" does not exist"
                    )));
                };
                targets.extend(targets_in_schema(interp, ns, objtype));
            }
        }
        GrantTargetType::AclTargetObject => {
            for obj in &stmt.objects {
                if let Some(target) = resolve_target(interp, objtype, obj.as_ref(), stmt.is_grant)?
                {
                    targets.push(target);
                }
            }
        }
        _ => return Ok(()),
    }
    let grantees = resolve_grantees(interp, &stmt.grantees)?;
    let privileges = check_privileges(objtype, &stmt.privileges, false)?;
    for target in &targets {
        check_target(interp, objtype, target, &privileges)?;
    }
    check_grant_option(stmt, &grantees)?;
    let behavior = stmt.behavior;
    let current = current_role(interp);
    for target in &targets {
        apply_target_grant(
            interp,
            target,
            &privileges,
            &grantees,
            stmt,
            behavior,
            current,
        )?;
    }
    Ok(())
}

/// getRelationsInNamespace / objectsInSchemaToOids.
fn targets_in_schema(interp: &PgCatalog, ns: PgNamespaceOid, objtype: ObjectType) -> Vec<Target> {
    let mut out: Vec<Target> = Vec::new();
    match objtype {
        ObjectType::ObjectTable => {
            let mut rels: Vec<PgClassOid> = interp
                .pg_class
                .values()
                .filter(|c| c.relnamespace == ns)
                .filter(|c| {
                    matches!(
                        c.relkind,
                        RelKind::Table
                            | RelKind::View
                            | RelKind::MaterializedView
                            | RelKind::ForeignTable
                            | RelKind::Partitioned
                    )
                })
                .map(|c| c.oid)
                .collect();
            rels.sort();
            out.extend(rels.into_iter().map(Target::Relation));
        }
        ObjectType::ObjectSequence => {
            let mut rels: Vec<PgClassOid> = interp
                .pg_class
                .values()
                .filter(|c| c.relnamespace == ns && c.relkind == RelKind::Sequence)
                .map(|c| c.oid)
                .collect();
            rels.sort();
            out.extend(rels.into_iter().map(Target::Relation));
        }
        ObjectType::ObjectFunction | ObjectType::ObjectProcedure | ObjectType::ObjectRoutine => {
            let mut procs: Vec<PgProcOid> = interp
                .pg_proc
                .values()
                .filter(|p| p.pronamespace == ns)
                .filter(|p| match objtype {
                    ObjectType::ObjectProcedure => {
                        p.prokind == crate::pg_catalog::ProKind::Procedure
                    }
                    ObjectType::ObjectFunction => {
                        p.prokind != crate::pg_catalog::ProKind::Procedure
                    }
                    _ => true,
                })
                .map(|p| p.oid)
                .collect();
            procs.sort();
            out.extend(procs.into_iter().map(Target::Function));
        }
        _ => {}
    }
    out
}

/// objectNamesToOids / get_object_address for one GRANT target.
fn resolve_target(
    interp: &mut PgCatalog,
    objtype: ObjectType,
    obj: Option<&typedpg_pg_query::ast::Node>,
    is_grant: bool,
) -> Result<Option<Target>, DdlError> {
    use ObjectType as O;
    let missing = |what: &str, name: &str| {
        Err(DdlError::TypeNotFound(format!(
            "{what} \"{name}\" does not exist"
        )))
    };
    Ok(Some(match (objtype, obj) {
        (O::ObjectTable | O::ObjectSequence, Some(ast::Node::RangeVar(rv))) => {
            Target::Relation(super::util::lookup_relation(interp, rv)?.1)
        }
        (
            O::ObjectFunction | O::ObjectProcedure | O::ObjectRoutine | O::ObjectAggregate,
            Some(ast::Node::ObjectWithArgs(owa)),
        ) => match super::functions::lookup_func_with_args(interp, objtype, owa, false)? {
            Some(oid) => Target::Function(oid),
            None => Target::Other,
        },
        (O::ObjectSchema, _) => {
            let Some(schema) = node_string(obj) else {
                return Ok(None);
            };
            let Some(ns) = interp.namespace_oid(schema) else {
                return Err(DdlError::TableNotFound(format!(
                    "schema \"{schema}\" does not exist"
                )));
            };
            Target::Schema(ns)
        }
        (O::ObjectType | O::ObjectDomain, Some(ast::Node::List(l))) => {
            let tn = typedpg_pg_query::ast::TypeName {
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
            let Some(lang) = interp.pg_language.values().find(|l| l.lanname == name) else {
                return missing("language", name);
            };
            Target::Language(lang.oid)
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
                ast::Node::Integer(i) => u32::try_from(i.ival).ok(),
                ast::Node::Float(f) => f.fval.parse().ok(),
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
                    if column_attnum(interp, *relid, column).is_none() {
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
            if t.typtype == TypType::Multirange {
                return Err(DdlError::Parse(
                    "cannot set privileges of multirange types (Set the privileges of the range \
                     type instead.)"
                        .into(),
                ));
            }
        }
        // lanpltrusted: C and internal functions are superuser-only.
        Target::Language(oid) if interp.pg_language.get(oid).is_some_and(|l| !l.lanpltrusted) => {
            let name = interp
                .pg_language
                .get(oid)
                .map_or("?", |l| l.lanname.as_str());
            return Err(DdlError::Parse(format!(
                "language \"{name}\" is not trusted (GRANT and REVOKE are not allowed on \
                 untrusted languages, because only superusers can use untrusted languages.)"
            )));
        }
        _ => {}
    }
    Ok(())
}

/// `get_attnum`, system columns included.
fn column_attnum(interp: &PgCatalog, relid: PgClassOid, column: &str) -> Option<i16> {
    interp
        .attribute_by_name(relid, column)
        .map(|a| a.attnum)
        .or_else(|| {
            crate::pg_catalog::SYSTEM_COLUMNS
                .iter()
                .find(|(n, ..)| *n == column)
                .map(|(_, _, attnum)| *attnum)
        })
}

/// Apply one GRANT / REVOKE to one target: `ExecGrant_*`.
fn apply_target_grant(
    interp: &mut PgCatalog,
    target: &Target,
    privileges: &Privileges<'_>,
    grantees: &[Grantee],
    stmt: &GrantStmt,
    behavior: DropBehavior,
    current: RoleId,
) -> Result<(), DdlError> {
    let object = match target {
        Target::Relation(relid) => AclObject::Relation(*relid),
        Target::Function(oid) => AclObject::Function(*oid),
        Target::Schema(ns) => AclObject::Schema(*ns),
        Target::Type(oid) => AclObject::Type(*oid),
        Target::Language(oid) => AclObject::Language(*oid),
        Target::Other => return Ok(()),
    };
    let kind = object.kind(interp);
    // `GRANT ALL ON TABLE seq` means the sequence's rights.
    let mask = if privileges.all {
        kind.all_rights()
    } else {
        privileges.mask
    };
    let owner = interp.owner_of(object);
    // select_best_grantor: the owner (a superuser, or a role with the owner's
    // privileges) grants as the owner; another grantor grants as itself.
    let grantor = if current == owner
        || interp.roles.is_superuser(current)
        || interp.roles.has_privs_of_role(current, owner)
    {
        owner
    } else {
        current
    };
    if mask != 0 {
        let old_acl = interp.acl_of(object);
        let new_acl = merge_acl_with_grant(
            interp,
            &old_acl,
            stmt.is_grant,
            stmt.grant_option,
            behavior,
            grantees,
            mask,
            grantor,
            owner,
        )?;
        interp.acls.items.insert(object, new_acl);
    }
    // Column privileges: explicit lists, and a table-level REVOKE's implicit
    // revoke (`ExecGrant_Relation`, the SQL standard's rule).
    if let Target::Relation(relid) = target {
        for (mask, columns) in &privileges.columns {
            let mask = mask.unwrap_or(bits::COLUMN);
            for column in columns {
                let Some(attnum) = column_attnum(interp, *relid, column) else {
                    continue;
                };
                apply_column_grant(
                    interp, *relid, attnum, mask, grantees, stmt, behavior, grantor, owner,
                )?;
            }
        }
        if !stmt.is_grant {
            let column_mask = mask & bits::COLUMN;
            if column_mask != 0 {
                let mut attnums: Vec<i16> = interp
                    .attributes_of(*relid)
                    .iter()
                    .map(|a| a.attnum)
                    .collect();
                attnums.extend(crate::pg_catalog::SYSTEM_COLUMNS.iter().map(|(_, _, n)| *n));
                for attnum in attnums {
                    apply_column_grant(
                        interp,
                        *relid,
                        attnum,
                        column_mask,
                        grantees,
                        stmt,
                        behavior,
                        grantor,
                        owner,
                    )?;
                }
            }
        }
    }
    Ok(())
}

/// `ExecGrant_Attribute` for one column.
#[allow(clippy::too_many_arguments)]
fn apply_column_grant(
    interp: &mut PgCatalog,
    relid: PgClassOid,
    attnum: i16,
    privileges: u32,
    grantees: &[Grantee],
    stmt: &GrantStmt,
    behavior: DropBehavior,
    grantor: RoleId,
    owner: RoleId,
) -> Result<(), DdlError> {
    let old_acl = interp.column_acl_of(relid, attnum);
    let new_acl = merge_acl_with_grant(
        interp,
        &old_acl,
        stmt.is_grant,
        stmt.grant_option,
        behavior,
        grantees,
        privileges,
        grantor,
        owner,
    )?;
    if new_acl.is_empty() {
        interp.acls.columns.remove(&(relid, attnum));
    } else {
        interp.acls.columns.insert((relid, attnum), new_acl);
    }
    Ok(())
}

/// `merge_acl_with_grant` / `aclupdate` over the catalog's role graph.
#[allow(clippy::too_many_arguments)]
fn merge_acl_with_grant(
    interp: &PgCatalog,
    old_acl: &[AclItem],
    is_grant: bool,
    grant_option: bool,
    behavior: DropBehavior,
    grantees: &[Grantee],
    privileges: u32,
    grantor: RoleId,
    owner: RoleId,
) -> Result<Vec<AclItem>, DdlError> {
    let mut new_acl = old_acl.to_vec();
    for &grantee in grantees {
        // The asymmetry comes from the spec: GRANT WITH GRANT OPTION grants
        // both the privilege and its option; plain REVOKE removes both, and
        // REVOKE GRANT OPTION FOR removes only the option.
        let item = AclItem {
            grantee,
            grantor,
            privileges: if is_grant || !grant_option {
                privileges
            } else {
                0
            },
            grant_options: if !is_grant || grant_option {
                privileges
            } else {
                0
            },
        };
        new_acl = acl_update(interp, &new_acl, &item, is_grant, owner, behavior)?;
    }
    Ok(new_acl)
}

// ─── ALTER DEFAULT PRIVILEGES ───────────────────────────────────────────────

/// ALTER DEFAULT PRIVILEGES [FOR ROLE ...] [IN SCHEMA ...] GRANT / REVOKE
/// (ExecAlterDefaultPrivilegesStmt, SetDefaultACLsInSchemas, SetDefaultACL).
pub fn alter_default_privileges(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::ast::AlterDefaultPrivilegesStmt,
) -> Result<(), DdlError> {
    let mut schemas: Vec<PgNamespaceOid> = Vec::new();
    let mut roles: Vec<RoleId> = Vec::new();
    for opt in &stmt.options {
        let Some(ast::Node::DefElem(de)) = opt.as_ref() else {
            continue;
        };
        match de.defname.as_str() {
            "schemas" => {
                let Some(ast::Node::List(list)) = de.arg.as_ref() else {
                    continue;
                };
                for item in &list.items {
                    let Some(name) = node_string(item.as_ref()) else {
                        continue;
                    };
                    let Some(ns) = interp.namespace_oid(name) else {
                        return Err(DdlError::TableNotFound(format!(
                            "schema \"{name}\" does not exist"
                        )));
                    };
                    schemas.push(ns);
                }
            }
            "roles" => {
                let Some(ast::Node::List(list)) = de.arg.as_ref() else {
                    continue;
                };
                for item in &list.items {
                    let Some(ast::Node::RoleSpec(spec)) = item.as_ref() else {
                        continue;
                    };
                    roles.push(resolve_role_spec(interp, spec)?);
                }
            }
            _ => {}
        }
    }
    if roles.is_empty() {
        roles.push(current_role(interp));
    }
    let Some(action) = stmt.action.as_ref() else {
        return Ok(());
    };
    let objtype = action.objtype;
    let grantees = resolve_grantees(interp, &action.grantees)?;
    let privileges = check_privileges(objtype, &action.privileges, true)?;
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
    check_grant_option(action, &grantees)?;
    let Some(class) = DefaultAclClass::of(objtype) else {
        return Ok(());
    };
    let behavior = action.behavior;
    let mask = if privileges.all {
        class.kind().all_rights()
    } else {
        privileges.mask
    };
    let schema_targets: Vec<Option<PgNamespaceOid>> = if schemas.is_empty() {
        vec![None]
    } else {
        schemas.iter().map(|&ns| Some(ns)).collect()
    };
    for role in roles {
        for schema in &schema_targets {
            // A global entry starts from the hard-wired default, a
            // per-schema one from an empty ACL (SetDefaultACL).
            let old_acl = interp
                .acls
                .defaults
                .get(&(role, *schema, class))
                .cloned()
                .unwrap_or_else(|| {
                    if schema.is_none() {
                        acldefault(class.kind(), role)
                    } else {
                        Vec::new()
                    }
                });
            let new_acl = merge_acl_with_grant(
                interp,
                &old_acl,
                action.is_grant,
                action.grant_option,
                behavior,
                &grantees,
                mask,
                role,
                role,
            )?;
            interp.acls.set_default_acl(role, *schema, class, new_acl);
        }
    }
    Ok(())
}

// ─── DROP OWNED BY ──────────────────────────────────────────────────────────

/// `DROP OWNED BY role`: revoke the grants the role made or holds, remove its
/// default privileges, then drop the objects it owns (`shdepDropOwned`).
pub(crate) fn drop_owned_by(
    interp: &mut PgCatalog,
    role: RoleId,
    _cascade: bool,
) -> Result<(), DdlError> {
    // ACLs first: every entry the role granted or received goes.
    interp.acls.defaults.retain(|key, acl| {
        let keep = key.0 != role;
        if !keep {
            return false;
        }
        acl.retain(|item| item.grantee != Grantee::Role(role) && item.grantor != role);
        true
    });
    interp.acls.defaults.retain(|_, acl| !acl.is_empty());
    for acl in interp.acls.items.values_mut() {
        acl.retain(|item| item.grantee != Grantee::Role(role) && item.grantor != role);
    }
    interp.acls.items.retain(|_, acl| !acl.is_empty());
    for acl in interp.acls.columns.values_mut() {
        acl.retain(|item| item.grantee != Grantee::Role(role) && item.grantor != role);
    }
    interp.acls.columns.retain(|_, acl| !acl.is_empty());

    // The relations: plain ones first, so their indexes and owned sequences
    // go with them.
    let mut relations: Vec<PgClassOid> = interp
        .pg_class
        .values()
        .filter(|c| interp.owner_of(AclObject::Relation(c.oid)) == role)
        .filter(|c| {
            matches!(
                c.relkind,
                RelKind::Table
                    | RelKind::Partitioned
                    | RelKind::View
                    | RelKind::MaterializedView
                    | RelKind::ForeignTable
                    | RelKind::Sequence
            )
        })
        .map(|c| c.oid)
        .collect();
    relations.sort();
    for relid in relations {
        super::drop::drop_relation_by_oid(interp, relid);
    }
    // Indexes and composite-row relations of owned types left over.
    let mut leftovers: Vec<PgClassOid> = interp
        .pg_class
        .values()
        .filter(|c| interp.owner_of(AclObject::Relation(c.oid)) == role)
        .map(|c| c.oid)
        .collect();
    leftovers.sort();
    for relid in leftovers {
        interp.remove_pg_class(relid);
    }

    let mut functions: Vec<PgProcOid> = interp
        .pg_proc
        .values()
        .filter(|p| interp.owner_of(AclObject::Function(p.oid)) == role)
        .map(|p| p.oid)
        .collect();
    functions.sort();
    super::drop::drop_functions_cascade(interp, &functions);

    let mut types: Vec<PgTypeOid> = interp
        .pg_type
        .values()
        .filter(|t| interp.owner_of(AclObject::Type(t.oid)) == role)
        // Arrays and multiranges go with their element / range type.
        .filter(|t| !is_dependent_type(interp, t.oid))
        .map(|t| t.oid)
        .collect();
    types.sort();
    for typ in types {
        super::drop::drop_type_cascade(interp, typ);
    }

    let mut schemas: Vec<PgNamespaceOid> = interp
        .pg_namespace
        .values()
        .filter(|n| interp.owner_of(AclObject::Schema(n.oid)) == role)
        .map(|n| n.oid)
        .collect();
    schemas.sort();
    for ns in schemas {
        let name = interp.namespace_name(ns).unwrap_or("?").to_owned();
        // What is left in the schema isn't owned by the role: PG's RESTRICT
        // errors, CASCADE drops it too.
        let mut still: Vec<PgClassOid> = interp
            .pg_class
            .values()
            .filter(|c| c.relnamespace == ns)
            .map(|c| c.oid)
            .collect();
        still.sort();
        let procs: Vec<PgProcOid> = interp
            .pg_proc
            .values()
            .filter(|p| p.pronamespace == ns)
            .map(|p| p.oid)
            .collect();
        let types: Vec<PgTypeOid> = interp
            .pg_type
            .values()
            .filter(|t| t.typnamespace == ns)
            .map(|t| t.oid)
            .collect();
        if !still.is_empty() || !procs.is_empty() || !types.is_empty() {
            if !_cascade {
                return Err(DdlError::DependencyError(format!(
                    "cannot drop schema {name} because other objects depend on it"
                )));
            }
            for relid in still.drain(..) {
                super::drop::drop_relation_by_oid(interp, relid);
            }
            super::drop::drop_functions_cascade(interp, &procs);
            for typ in types {
                if !is_dependent_type(interp, typ) {
                    super::drop::drop_type_cascade(interp, typ);
                }
            }
        }
        let _ = interp.remove_pg_namespace(ns);
    }
    Ok(())
}

/// `IsTrueArrayType` / a multirange: types whose privileges and lifecycle
/// follow another type.
fn is_dependent_type(interp: &PgCatalog, oid: PgTypeOid) -> bool {
    let Some(t) = interp.get_type(oid) else {
        return false;
    };
    if t.typtype == TypType::Multirange {
        return true;
    }
    t.typcategory == crate::pg_catalog::TypCategory::Array
        && t.typelem.is_some()
        && t.typelem.and_then(|e| interp.array_type_of(e)) == Some(oid)
}

// ─── has_*_privilege ────────────────────────────────────────────────────────

impl PgCatalog {
    /// The role named `name`, if the catalog knows it (see
    /// [`PgCatalog::role_exists`] for what counts).
    fn known_role(&self, name: &str) -> Option<RoleId> {
        if name == PgCatalog::MIGRATIONS_USER {
            return Some(self.roles.migrations_user);
        }
        self.roles.id(name)
    }

    fn acl_kind_privilege(
        &self,
        role: &str,
        object: AclObject,
        allowed: u32,
        privilege: Privilege,
    ) -> bool {
        let Some(role_id) = self.known_role(role) else {
            return false;
        };
        let mask = privilege.bit();
        if mask & !allowed != 0 {
            return false;
        }
        if self.roles.is_superuser(role_id) {
            return true;
        }
        let owner = self.owner_of(object);
        let acl = self.acl_of(object);
        aclmask_privileges(self, role_id, owner, &acl, mask) & mask == mask
    }

    /// `has_table_privilege(role, relid, privilege)` (`pg_class_aclmask`):
    /// the privileges of a relation — a table, view, materialized view,
    /// foreign table, partitioned table or sequence. Like PG's function, it
    /// accepts any relation OID; a sequence's ACL is its own kind's.
    pub fn has_table_privilege(&self, role: &str, relid: PgClassOid, privilege: Privilege) -> bool {
        let Some(class) = self.pg_class.get(&relid) else {
            return false;
        };
        let Some(role_id) = self.known_role(role) else {
            return false;
        };
        let mask = privilege.bit();
        if mask & !bits::RELATION != 0 {
            return false;
        }
        if self.roles.is_superuser(role_id) {
            return true;
        }
        // PG denies writes on system catalogs to every non-superuser, even
        // with an ACL (`pg_class_aclmask_ext`).
        let mut mask = mask;
        if mask & (bits::INSERT | bits::UPDATE | bits::DELETE | bits::TRUNCATE | bits::USAGE) != 0
            && self.is_system_class(relid)
            && class.relkind != RelKind::View
        {
            mask &= !(bits::INSERT | bits::UPDATE | bits::DELETE | bits::TRUNCATE | bits::USAGE);
        }
        let object = AclObject::Relation(relid);
        let owner = self.owner_of(object);
        let acl = self.acl_of(object);
        let mut result = aclmask_privileges(self, role_id, owner, &acl, mask);
        if mask & bits::SELECT != 0
            && result & bits::SELECT == 0
            && self.has_predefined(role_id, "pg_read_all_data")
        {
            result |= bits::SELECT;
        }
        if mask & (bits::INSERT | bits::UPDATE | bits::DELETE) != 0
            && result & (bits::INSERT | bits::UPDATE | bits::DELETE) == 0
            && self.has_predefined(role_id, "pg_write_all_data")
        {
            result |= mask & (bits::INSERT | bits::UPDATE | bits::DELETE);
        }
        if mask & bits::MAINTAIN != 0
            && result & bits::MAINTAIN == 0
            && self.has_predefined(role_id, "pg_maintain")
        {
            result |= bits::MAINTAIN;
        }
        result & mask == mask
    }

    /// `has_sequence_privilege`: a sequence's USAGE / SELECT / UPDATE.
    pub fn has_sequence_privilege(
        &self,
        role: &str,
        relid: PgClassOid,
        privilege: Privilege,
    ) -> bool {
        if self.pg_class.get(&relid).map(|c| c.relkind) != Some(RelKind::Sequence) {
            return false;
        }
        let Some(role_id) = self.known_role(role) else {
            return false;
        };
        let mask = privilege.bit();
        if mask & !bits::SEQUENCE != 0 {
            return false;
        }
        if self.roles.is_superuser(role_id) {
            return true;
        }
        let object = AclObject::Relation(relid);
        let owner = self.owner_of(object);
        let acl = self.acl_of(object);
        let mut result = aclmask_privileges(self, role_id, owner, &acl, mask);
        if mask & bits::SELECT != 0
            && result & bits::SELECT == 0
            && self.has_predefined(role_id, "pg_read_all_data")
        {
            result |= bits::SELECT;
        }
        if mask & bits::UPDATE != 0
            && result & bits::UPDATE == 0
            && self.has_predefined(role_id, "pg_write_all_data")
        {
            result |= bits::UPDATE;
        }
        result & mask == mask
    }

    /// `has_column_privilege`: the privilege at the table level, or on the
    /// column (`pg_attribute.attacl`). Unlike the ACL entry itself, the
    /// table's ACL applies to every column.
    pub fn has_column_privilege(
        &self,
        role: &str,
        relid: PgClassOid,
        attnum: i16,
        privilege: Privilege,
    ) -> bool {
        let mask = privilege.bit();
        if mask & !bits::COLUMN != 0 {
            return false;
        }
        if self.has_table_level_privilege(role, relid, privilege) {
            return true;
        }
        let Some(role_id) = self.known_role(role) else {
            return false;
        };
        if self.roles.is_superuser(role_id) {
            return true;
        }
        let owner = self.owner_of(AclObject::Relation(relid));
        let acl = self.column_acl_of(relid, attnum);
        aclmask_privileges(self, role_id, owner, &acl, mask) & mask == mask
    }

    /// `has_any_column_privilege`: the privilege on the table or on any of
    /// its columns.
    pub fn has_any_column_privilege(
        &self,
        role: &str,
        relid: PgClassOid,
        privilege: Privilege,
    ) -> bool {
        let mask = privilege.bit();
        if mask & !bits::COLUMN != 0 {
            return false;
        }
        if self.has_table_level_privilege(role, relid, privilege) {
            return true;
        }
        let Some(role_id) = self.known_role(role) else {
            return false;
        };
        if self.roles.is_superuser(role_id) {
            return true;
        }
        let owner = self.owner_of(AclObject::Relation(relid));
        self.attributes_of(relid)
            .iter()
            .filter(|a| a.attnum > 0)
            .any(|a| {
                let acl = self.column_acl_of(relid, a.attnum);
                aclmask_privileges(self, role_id, owner, &acl, mask) & mask == mask
            })
    }

    /// The table-level half of `has_column_privilege` /
    /// `has_any_column_privilege` (`pg_class_aclcheck`): the same check as
    /// `has_table_privilege`, including the system-catalog write mask.
    fn has_table_level_privilege(
        &self,
        role: &str,
        relid: PgClassOid,
        privilege: Privilege,
    ) -> bool {
        self.has_table_privilege(role, relid, privilege)
    }

    /// `has_function_privilege`: EXECUTE on a function, procedure or
    /// aggregate.
    pub fn has_function_privilege(&self, role: &str, proc_oid: PgProcOid) -> bool {
        if !self.pg_proc.contains_key(&proc_oid) {
            return false;
        }
        self.acl_kind_privilege(
            role,
            AclObject::Function(proc_oid),
            bits::FUNCTION,
            Privilege::Execute,
        )
    }

    /// `has_schema_privilege`: USAGE / CREATE on a schema. `pg_read_all_data`
    /// and `pg_write_all_data` imply USAGE on every schema.
    pub fn has_schema_privilege(
        &self,
        role: &str,
        nsoid: PgNamespaceOid,
        privilege: Privilege,
    ) -> bool {
        if self.acl_kind_privilege(role, AclObject::Schema(nsoid), bits::SCHEMA, privilege) {
            return true;
        }
        if privilege != Privilege::Usage {
            return false;
        }
        let Some(role_id) = self.known_role(role) else {
            return false;
        };
        self.has_predefined(role_id, "pg_read_all_data")
            || self.has_predefined(role_id, "pg_write_all_data")
    }

    /// `has_type_privilege`: USAGE on a type or domain. An array's
    /// privileges are its element type's; a multirange's are its range
    /// type's.
    pub fn has_type_privilege(&self, role: &str, type_oid: PgTypeOid) -> bool {
        let Some(oid) = self.acl_type_of(type_oid) else {
            return false;
        };
        self.acl_kind_privilege(role, AclObject::Type(oid), bits::TYPE, Privilege::Usage)
    }

    /// The type whose ACL `type_oid` uses (`pg_type_aclmask_ext`).
    fn acl_type_of(&self, mut oid: PgTypeOid) -> Option<PgTypeOid> {
        loop {
            let ty = self.get_type(oid)?;
            if ty.typtype == TypType::Multirange {
                let Some(range) = self
                    .pg_range
                    .values()
                    .find(|r| r.rngmultitypid == Some(oid))
                    .map(|r| r.rngtypid)
                else {
                    return Some(oid);
                };
                oid = range;
                continue;
            }
            if ty.typcategory == crate::pg_catalog::TypCategory::Array
                && ty.typelem.is_some()
                && ty.typelem.and_then(|e| self.array_type_of(e)) == Some(oid)
            {
                oid = ty.typelem?;
                continue;
            }
            return Some(oid);
        }
    }

    fn has_predefined(&self, role: RoleId, predefined: &str) -> bool {
        self.roles
            .id(predefined)
            .is_some_and(|id| self.roles.has_privs_of_role(role, id))
    }

    /// Load one `pg_acl` seed row: the ACL of a built-in object, as
    /// `aclitem[]` text. Unknown role names intern as external roles (the
    /// seed's grants name `postgres`, the predefined roles and, in principle,
    /// roles an extension's script created).
    pub(crate) fn load_seed_acl(&mut self, row: &SeedAcl) {
        let Some(items) = parse_acl_text(&row.acl) else {
            return;
        };
        let mut acl = Vec::with_capacity(items.len());
        for ParsedAclItem {
            grantee,
            grantor,
            privileges,
            grant_options,
        } in items
        {
            let grantee = match grantee {
                Some(name) => Grantee::Role(self.roles.id_or_intern(&name)),
                None => Grantee::Public,
            };
            let Some(grantor) = grantor else {
                continue;
            };
            acl.push(AclItem {
                grantee,
                grantor: self.roles.id_or_intern(&grantor),
                privileges,
                grant_options,
            });
        }
        let object = match row.catalog {
            AclCatalog::Class => AclObject::Relation(PgClassOid::from_nonzero(
                std::num::NonZeroU32::new(row.objid.get()).expect("seed acl oid"),
            )),
            AclCatalog::Proc => AclObject::Function(PgProcOid::from_nonzero(
                std::num::NonZeroU32::new(row.objid.get()).expect("seed acl oid"),
            )),
            AclCatalog::Namespace => AclObject::Schema(PgNamespaceOid::from_nonzero(
                std::num::NonZeroU32::new(row.objid.get()).expect("seed acl oid"),
            )),
            AclCatalog::Type => AclObject::Type(PgTypeOid::from_nonzero(
                std::num::NonZeroU32::new(row.objid.get()).expect("seed acl oid"),
            )),
            AclCatalog::Language => AclObject::Language(PgLanguageOid::from_nonzero(
                std::num::NonZeroU32::new(row.objid.get()).expect("seed acl oid"),
            )),
            AclCatalog::Attribute => {
                let relid = PgClassOid::from_nonzero(
                    std::num::NonZeroU32::new(row.objid.get()).expect("seed acl oid"),
                );
                if !acl.is_empty() {
                    self.acls.columns.insert((relid, row.objsubid), acl);
                }
                return;
            }
        };
        if !acl.is_empty() {
            self.acls.items.insert(object, acl);
        }
    }
}

/// One `aclitem` of [`parse_acl_text`]'s output, by role name.
struct ParsedAclItem {
    /// `None` is PUBLIC.
    grantee: Option<String>,
    grantor: Option<String>,
    privileges: u32,
    grant_options: u32,
}

/// Parse PG's `aclitem[]` text — `{grantee=privs/grantor,...}` — into
/// its items. A missing grantee is
/// PUBLIC; a letter followed by `*` carries its grant option
/// (`aclitemout` / `aclparse`). `None` on malformed text, which the seed
/// exporter never writes.
fn parse_acl_text(text: &str) -> Option<Vec<ParsedAclItem>> {
    let text = text.trim();
    let inner = text.strip_prefix('{')?.strip_suffix('}')?;
    let mut entries = Vec::new();
    let mut start = 0usize;
    let mut in_quotes = false;
    for (i, c) in inner.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                entries.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < inner.len() {
        entries.push(&inner[start..]);
    }
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        // Split on the first `=` outside quotes, then on `/` outside quotes.
        let (grantee_text, rest) = split_acl_part(entry, '=')?;
        let (privileges_text, grantor_text) = split_acl_part(rest, '/')?;
        let grantee = decode_acl_name(grantee_text.trim());
        let grantor = decode_acl_name(grantor_text.trim());
        let mut privileges = 0u32;
        let mut grant_options = 0u32;
        let mut chars = privileges_text.chars();
        while let Some(c) = chars.next() {
            let bit = match c {
                'a' => bits::INSERT,
                'r' => bits::SELECT,
                'w' => bits::UPDATE,
                'd' => bits::DELETE,
                'D' => bits::TRUNCATE,
                'x' => bits::REFERENCES,
                't' => bits::TRIGGER,
                'X' => bits::EXECUTE,
                'U' => bits::USAGE,
                'C' => bits::CREATE,
                'T' => bits::CREATE_TEMP,
                'c' => bits::CONNECT,
                's' => bits::SET,
                'A' => bits::ALTER_SYSTEM,
                'm' => bits::MAINTAIN,
                _ => return None,
            };
            privileges |= bit;
            if chars.clone().next() == Some('*') {
                chars.next();
                grant_options |= bit;
            }
        }
        out.push(ParsedAclItem {
            grantee,
            grantor,
            privileges,
            grant_options,
        });
    }
    Some(out)
}

/// Split an ACL entry at `separator`, outside double quotes, decoding
/// `""` escapes and trimming. An empty side is `None` (PUBLIC / no grantor).
fn split_acl_part(text: &str, separator: char) -> Option<(&str, &str)> {
    let bytes = text.as_bytes();
    let mut in_quotes = false;
    let mut at = None;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'"' => in_quotes = !in_quotes,
            _ if !in_quotes && b as char == separator => {
                at = Some(i);
                break;
            }
            _ => {}
        }
    }
    let i = at?;
    let (left, right) = text.split_at(i);
    let right = &right[separator.len_utf8()..];
    Some((left, right))
}

/// A role name in an `aclitem`: empty is `None` (PUBLIC / no grantor); a
/// `"..."` name decodes its `""` quote escapes.
fn decode_acl_name(text: &str) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    match text.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
        Some(inner) => Some(inner.replace("\"\"", "\"")),
        None => Some(text.to_owned()),
    }
}
