//! The privilege checks PostgreSQL makes of a DDL statement as the current
//! role: `CREATE` on the schema a new object goes in (and on the database,
//! for a new schema), ownership of what an `ALTER`, `DROP`, `COMMENT` or
//! `CREATE INDEX` changes, `USAGE` on the schemas a statement names and the
//! types it uses, `REFERENCES` on a foreign key's referenced table, and
//! what `OWNER TO` / `AUTHORIZATION` need of the new owner.
//!
//! They apply only when the current role (`SET ROLE`, `SET SESSION
//! AUTHORIZATION`) is one whose attributes the catalog knows — a role the
//! migrations created, or a predefined one — and that is not a superuser.
//! The migrations' own user is superuser-equivalent, and a role the
//! migrations merely name may be a superuser: nothing is checked for them.
//! Nor once what may differ from the catalog covers the role (see
//! [`super::uncertain`]); the privilege lookups themselves read the
//! catalog's upper bound, and an owner that may differ passes. A check
//! that may pass passes.
//!
//! The checks run before the statement is applied, and only for objects
//! that exist: a statement naming a missing object fails as it would
//! anyway.

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{self, AlterTableType, ConstrType, Node, ObjectType};

use super::DdlError;
use super::acl::{AclObject, Privilege};
use super::roles::RoleId;
use crate::oid::{PgClassOid, PgNamespaceOid, PgTypeOid};
use crate::pg_catalog::{PgCatalog, RelKind};

/// Check `stmt`'s privileges as the current role (see the module docs).
pub(crate) fn check_statement(db: &mut PgCatalog, stmt: &Node) -> Result<(), DdlError> {
    let Some(role) = restricted_role(db) else {
        return Ok(());
    };
    let name = db.roles.name(role).to_owned();
    Checker { db, role, name }.statement(stmt)
}

thread_local! {
    /// How many extension scripts are running: an extension's script runs
    /// as the bootstrap superuser for a trusted extension, and only a
    /// superuser may install another (`execute_extension_script`).
    static SUSPENDED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Run `f` (an extension's script) with no privilege checks.
pub(crate) fn suspended<R>(f: impl FnOnce() -> R) -> R {
    struct Resume;
    impl Drop for Resume {
        fn drop(&mut self) {
            SUSPENDED.with(|s| s.set(s.get() - 1));
        }
    }
    SUSPENDED.with(|s| s.set(s.get() + 1));
    let _resume = Resume;
    f()
}

/// The current role, when the checks apply to it: one whose attributes the
/// catalog knows (created by the migrations, or predefined), not a
/// superuser, and not covered by what may differ from the catalog.
pub(crate) fn restricted_role(db: &mut PgCatalog) -> Option<RoleId> {
    let role = super::roles::current_role(db);
    restricted(db, role).then_some(role)
}

/// Whether the checks apply to `role` (see [`restricted_role`]).
pub(crate) fn restricted(db: &PgCatalog, role: RoleId) -> bool {
    if SUSPENDED.with(std::cell::Cell::get) > 0 {
        return false;
    }
    let roles = &db.roles;
    if roles.same_real_role(role, roles.migrations_user)
        || roles.same_real_role(role, roles.bootstrap)
    {
        return false;
    }
    let Some(r) = roles.get(role) else {
        return false;
    };
    let known = r.created || super::session::PREDEFINED_ROLES.contains(&r.name.as_str());
    known && !r.superuser && !roles.uncertain.catalog && !roles.uncertain.roles.contains(&r.name)
}

struct Checker<'a> {
    db: &'a PgCatalog,
    role: RoleId,
    name: String,
}

/// `permission denied for <what>` (`aclcheck_error`, ACLCHECK_NO_PRIV).
fn denied(what: impl std::fmt::Display) -> DdlError {
    DdlError::UnsupportedDdl(format!("permission denied for {what}"))
}

/// `must be owner of <what>` (`aclcheck_error`, ACLCHECK_NOT_OWNER).
fn not_owner(what: impl std::fmt::Display) -> DdlError {
    DdlError::UnsupportedDdl(format!("must be owner of {what}"))
}

/// `get_relkind_objtype`'s word for a relation, as `aclcheck_error` says it.
fn relkind_word(relkind: RelKind) -> &'static str {
    match relkind {
        RelKind::View => "view",
        RelKind::MaterializedView => "materialized view",
        RelKind::Sequence => "sequence",
        RelKind::ForeignTable => "foreign table",
        RelKind::Index | RelKind::PartitionedIndex => "index",
        _ => "table",
    }
}

impl Checker<'_> {
    /// `object_ownercheck` / `has_privs_of_role(role, owner)`.
    fn owns(&self, object: AclObject) -> bool {
        !self.db.roles.uncertain.owners.is_empty()
            || self
                .db
                .roles
                .has_privs_of_role(self.role, self.db.owner_of(object))
    }

    /// `object_aclcheck` on a schema. The session's temporary schema grants
    /// what the database's TEMP privilege does (`pg_namespace_aclmask`).
    fn schema_allows(&self, role: &str, ns: PgNamespaceOid, privilege: Privilege) -> bool {
        self.db.temp_namespace == Some(ns) || self.db.has_schema_privilege(role, ns, privilege)
    }

    fn schema_name(&self, ns: PgNamespaceOid) -> String {
        self.db.namespace_name(ns).unwrap_or_default().to_owned()
    }

    fn relation(&self, relid: PgClassOid) -> Option<&crate::pg_catalog::PgClass> {
        self.db.pg_class.get(&relid)
    }

    /// `CREATE` on schema `ns` for the current role, or `permission denied
    /// for schema …`.
    fn create_in(&self, ns: PgNamespaceOid) -> Result<(), DdlError> {
        if self.schema_allows(&self.name, ns, Privilege::Create) {
            Ok(())
        } else {
            Err(denied(format!("schema {}", self.schema_name(ns))))
        }
    }

    /// The creation namespace of a new relation (`RangeVarGetCreationNamespace`):
    /// `None` for a temporary one (its schema is the session's own) or one
    /// whose schema is missing (the statement reports it).
    fn creation_namespace(&self, rv: &ast::RangeVar) -> Option<PgNamespaceOid> {
        if rv.relpersistence == "t" || rv.schemaname == "pg_temp" {
            return None;
        }
        let schema = if rv.schemaname.is_empty() {
            super::util::creation_schema(self.db).ok()?
        } else {
            rv.schemaname.clone()
        };
        self.db.namespace_oid(&schema)
    }

    /// The creation namespace of a new object named by a qualified name
    /// (`QualifiedNameGetCreationNamespace`).
    fn creation_namespace_of(&self, names: &[Option<Node>]) -> Option<PgNamespaceOid> {
        let names: Vec<&str> = names
            .iter()
            .filter_map(|n| super::util::node_string(n.as_ref()))
            .collect();
        let schema = match names.as_slice() {
            [_] => super::util::creation_schema(self.db).ok()?,
            [.., schema, _] => (*schema).to_owned(),
            [] => return None,
        };
        if schema == "pg_temp" {
            return None;
        }
        self.db.namespace_oid(&schema)
    }

    /// An existing relation a statement names.
    fn existing(&self, rv: &ast::RangeVar) -> Option<PgClassOid> {
        super::util::lookup_relation(self.db, rv)
            .ok()
            .map(|(_, relid)| relid)
    }

    /// Ownership of relation `relid`, named `written` in the statement
    /// (the RangeVar callbacks report the name as written).
    fn own_relation(&self, relid: PgClassOid, written: &str) -> Result<(), DdlError> {
        if self.owns(AclObject::Relation(relid)) {
            return Ok(());
        }
        let word = self
            .relation(relid)
            .map_or("table", |c| relkind_word(c.relkind));
        Err(not_owner(format!("{word} {written}")))
    }

    /// `USAGE` on type `oid` (`aclcheck_error_type`): an array's is its
    /// element type's.
    fn use_type(&self, oid: PgTypeOid) -> Result<(), DdlError> {
        if self.db.has_type_privilege(&self.name, oid) {
            return Ok(());
        }
        let shown = crate::expr::array_element_type(self.db, oid)
            .filter(|_| {
                self.db
                    .get_type(oid)
                    .is_some_and(|t| t.typtype != crate::pg_catalog::TypType::Domain)
            })
            .unwrap_or(oid);
        Err(denied(format!(
            "type {}",
            super::util::format_type_for_message(self.db, shown)
        )))
    }

    /// `USAGE` on the schema of every qualified name `stmt` looks up
    /// (`LookupExplicitNamespace`) — not a new object's own name, whose
    /// schema needs `CREATE` instead (`LookupCreationNamespace`).
    fn schema_usage(&self, stmt: &Node) -> Result<(), DdlError> {
        let created: Option<*const ast::RangeVar> = match stmt {
            Node::CreateStmt(s) => s.relation.as_deref().map(std::ptr::from_ref),
            Node::CreateSeqStmt(s) => s.sequence.as_deref().map(std::ptr::from_ref),
            Node::ViewStmt(s) => s.view.as_deref().map(std::ptr::from_ref),
            Node::CompositeTypeStmt(s) => s.typevar.as_deref().map(std::ptr::from_ref),
            Node::CreateTableAsStmt(s) => s
                .into
                .as_ref()
                .and_then(|i| i.rel.as_deref())
                .map(std::ptr::from_ref),
            _ => None,
        };
        let strings = |items: &[Option<Node>]| -> Vec<String> {
            items
                .iter()
                .filter_map(|n| super::util::node_string(n.as_ref()).map(str::to_owned))
                .collect()
        };
        let mut schemas: Vec<String> = Vec::new();
        for (node, _) in stmt.nodes() {
            let schema = match node {
                NodeRef::RangeVar(rv) if created != Some(std::ptr::from_ref(rv)) => {
                    Some(rv.schemaname.clone())
                }
                NodeRef::TypeName(t) => qualifier(strings(&t.names)),
                NodeRef::FuncCall(f) => qualifier(strings(&f.funcname)),
                NodeRef::ObjectWithArgs(o) => qualifier(strings(&o.objname)),
                _ => None,
            };
            if let Some(schema) = schema.filter(|s| !s.is_empty() && !schemas.contains(s)) {
                schemas.push(schema);
            }
        }
        // The relations a DROP or a COMMENT names, as qualified names.
        let object_lists: Vec<&Node> = match stmt {
            Node::DropStmt(d) if is_relation_kind(d.remove_type) => {
                d.objects.iter().flatten().collect()
            }
            Node::CommentStmt(c) if is_relation_kind(c.objtype) => c.object.iter().collect(),
            _ => Vec::new(),
        };
        for object in object_lists {
            if let Node::List(l) = object
                && let Some(schema) = qualifier(strings(&l.items))
                && !schemas.contains(&schema)
            {
                schemas.push(schema);
            }
        }
        for schema in schemas {
            if let Some(ns) = self.db.namespace_oid(&schema)
                && !self.schema_allows(&self.name, ns, Privilege::Usage)
            {
                return Err(denied(format!("schema {schema}")));
            }
        }
        Ok(())
    }

    fn statement(&self, stmt: &Node) -> Result<(), DdlError> {
        match stmt {
            Node::CreateStmt(s) => {
                self.schema_usage(stmt)?;
                self.create_table(s)
            }
            Node::CreateSeqStmt(s) => {
                self.schema_usage(stmt)?;
                match s
                    .sequence
                    .as_deref()
                    .and_then(|rv| self.creation_namespace(rv))
                {
                    Some(ns) => self.create_in(ns),
                    None => Ok(()),
                }
            }
            Node::ViewStmt(s) => {
                self.schema_usage(stmt)?;
                let Some(rv) = s.view.as_deref() else {
                    return Ok(());
                };
                // RangeVarGetAndCheckCreationNamespace first; then, for
                // CREATE OR REPLACE VIEW of an existing view, its owner.
                if let Some(ns) = self.creation_namespace(rv) {
                    self.create_in(ns)?;
                }
                match self.existing(rv) {
                    Some(relid) if s.replace => self.own_relation(relid, &rv.relname),
                    _ => Ok(()),
                }
            }
            Node::CreateTableAsStmt(s) => {
                self.schema_usage(stmt)?;
                match s
                    .into
                    .as_ref()
                    .and_then(|i| i.rel.as_deref())
                    .and_then(|rv| self.creation_namespace(rv))
                {
                    Some(ns) => self.create_in(ns),
                    None => Ok(()),
                }
            }
            Node::CompositeTypeStmt(s) => {
                self.schema_usage(stmt)?;
                match s
                    .typevar
                    .as_deref()
                    .and_then(|rv| self.creation_namespace(rv))
                {
                    Some(ns) => self.create_in(ns),
                    None => Ok(()),
                }
            }
            Node::CreateEnumStmt(s) => self.create_named(&s.type_name),
            Node::CreateRangeStmt(s) => self.create_named(&s.type_name),
            Node::CreateDomainStmt(s) => {
                self.schema_usage(stmt)?;
                self.create_named(&s.domainname)?;
                // DefineDomain: USAGE on the base type.
                if let Some(tn) = s.type_name.as_deref()
                    && let Some(base) = super::util::resolve_type_name(tn, self.db)
                {
                    self.use_type(base)?;
                }
                Ok(())
            }
            Node::DefineStmt(s) if s.kind == ObjectType::ObjectType => {
                self.create_named(&s.defnames)
            }
            Node::CreateFunctionStmt(s) => {
                self.schema_usage(stmt)?;
                self.create_function(s)
            }
            Node::IndexStmt(s) => {
                self.schema_usage(stmt)?;
                let Some(rv) = s.relation.as_deref() else {
                    return Ok(());
                };
                let Some(relid) = self.existing(rv) else {
                    return Ok(());
                };
                self.own_relation(relid, &rv.relname)?;
                // DefineIndex: CREATE on the table's schema.
                match self.relation(relid) {
                    Some(c) => self.create_in(c.relnamespace),
                    None => Ok(()),
                }
            }
            Node::AlterTableStmt(s) => {
                self.schema_usage(stmt)?;
                self.alter_table(s)
            }
            Node::RenameStmt(s) => {
                self.schema_usage(stmt)?;
                self.rename(s)
            }
            Node::DropStmt(s) => {
                self.schema_usage(stmt)?;
                self.drop(s)
            }
            Node::CommentStmt(s) => {
                self.schema_usage(stmt)?;
                self.comment(s)
            }
            Node::AlterOwnerStmt(s) => {
                self.schema_usage(stmt)?;
                self.alter_owner(s)
            }
            Node::CreateSchemaStmt(s) => self.create_schema(s),
            _ => Ok(()),
        }
    }

    /// A new object named by a qualified name: `CREATE` on its schema.
    fn create_named(&self, names: &[Option<Node>]) -> Result<(), DdlError> {
        match self.creation_namespace_of(names) {
            Some(ns) => self.create_in(ns),
            None => Ok(()),
        }
    }

    /// CREATE TABLE: CREATE on the schema, ownership of the parents
    /// (`MergeAttributes`), USAGE on the column types
    /// (`BuildDescForRelation`), REFERENCES on the referenced tables.
    fn create_table(&self, s: &ast::CreateStmt) -> Result<(), DdlError> {
        let Some(rv) = s.relation.as_deref() else {
            return Ok(());
        };
        let Some(ns) = self.creation_namespace(rv) else {
            return Ok(());
        };
        self.create_in(ns)?;
        // IF NOT EXISTS over an existing relation skips the rest.
        if s.if_not_exists && self.existing(rv).is_some() {
            return Ok(());
        }
        for parent in s.inh_relations.iter().flatten() {
            if let Node::RangeVar(prv) = parent
                && let Some(relid) = self.existing(prv)
            {
                self.own_relation(relid, &relation_name(self.db, relid, &prv.relname))?;
            }
        }
        for elt in s.table_elts.iter().flatten() {
            if let Node::ColumnDef(cd) = elt
                && let Some(tn) = cd.type_name.as_deref()
                && let Some(oid) = super::util::resolve_type_name(tn, self.db)
            {
                self.use_type(oid)?;
            }
        }
        let new_table = (rv.schemaname.as_str(), rv.relname.as_str());
        for elt in s.table_elts.iter().flatten() {
            match elt {
                Node::Constraint(c) => self.references(c, Some(new_table))?,
                Node::ColumnDef(cd) => {
                    for c in cd.constraints.iter().flatten() {
                        if let Node::Constraint(c) = c {
                            self.references(c, Some(new_table))?;
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// A foreign key: REFERENCES on the referenced table, or on each
    /// referenced column (`checkFkeyPermissions`). Not over the table being
    /// created (`new_table`: its schema as written and name), which the
    /// role owns.
    fn references(
        &self,
        c: &ast::Constraint,
        new_table: Option<(&str, &str)>,
    ) -> Result<(), DdlError> {
        if c.contype != ConstrType::ConstrForeign {
            return Ok(());
        }
        let Some(pk) = c.pktable.as_deref() else {
            return Ok(());
        };
        if new_table.is_some_and(|(schema, name)| {
            pk.relname == name && (pk.schemaname.is_empty() || pk.schemaname == schema)
        }) {
            return Ok(());
        }
        let Some(relid) = self.existing(pk) else {
            return Ok(());
        };
        if self
            .db
            .has_table_privilege(&self.name, relid, Privilege::References)
        {
            return Ok(());
        }
        // The referenced columns: the listed ones, else the primary key's.
        let columns: Vec<String> = if c.pk_attrs.is_empty() {
            match primary_key_columns(self.db, relid) {
                Some(cols) => cols,
                None => return Ok(()),
            }
        } else {
            c.pk_attrs
                .iter()
                .filter_map(|n| super::util::node_string(n.as_ref()).map(str::to_owned))
                .collect()
        };
        let attrs = self.db.attributes_of(relid);
        let all = columns.iter().all(|col| {
            attrs.iter().find(|a| a.attname == *col).is_none_or(|a| {
                self.db
                    .has_column_privilege(&self.name, relid, a.attnum, Privilege::References)
            })
        });
        if all {
            return Ok(());
        }
        let c = self.relation(relid);
        Err(denied(format!(
            "{} {}",
            c.map_or("table", |c| relkind_word(c.relkind)),
            c.map_or(pk.relname.as_str(), |c| c.relname.as_str())
        )))
    }

    /// CREATE FUNCTION: CREATE on the schema, USAGE on a trusted language
    /// (an untrusted one is a superuser's), and a replaced routine's owner.
    fn create_function(&self, s: &ast::CreateFunctionStmt) -> Result<(), DdlError> {
        let Some(ns) = self.creation_namespace_of(&s.funcname) else {
            return Ok(());
        };
        self.create_in(ns)?;
        let language = s
            .options
            .iter()
            .flatten()
            .find_map(|o| match o {
                Node::DefElem(de) if de.defname == "language" => {
                    super::util::node_string(de.arg.as_ref()).map(str::to_owned)
                }
                _ => None,
            })
            .or_else(|| s.sql_body.is_some().then(|| "sql".to_owned()));
        if let Some(language) = language
            && let Some(lang) = self.db.pg_language.values().find(|l| l.lanname == language)
        {
            let allowed = lang.lanpltrusted && self.db.has_language_privilege(&self.name, lang.oid);
            if !allowed {
                return Err(denied(format!("language {}", lang.lanname)));
            }
        }
        if s.replace {
            let name = s
                .funcname
                .iter()
                .filter_map(|n| super::util::node_string(n.as_ref()))
                .next_back()
                .unwrap_or_default();
            // The routine replaced: one of the name in the schema whose
            // input types are the new one's.
            let inputs: Option<Vec<PgTypeOid>> = s
                .parameters
                .iter()
                .flatten()
                .filter_map(|p| match p {
                    Node::FunctionParameter(fp)
                        if !matches!(
                            fp.mode,
                            ast::FunctionParameterMode::FuncParamOut
                                | ast::FunctionParameterMode::FuncParamTable
                        ) =>
                    {
                        Some(
                            fp.arg_type
                                .as_deref()
                                .and_then(|tn| super::util::resolve_type_name(tn, self.db)),
                        )
                    }
                    _ => None,
                })
                .collect();
            if let Some(inputs) = inputs
                && let Some(existing) = self
                    .db
                    .find_functions(self.db.namespace_name(ns), name)
                    .iter()
                    .find(|p| p.pronamespace == ns && p.proargtypes == inputs)
                && !self.owns(AclObject::Function(existing.oid))
            {
                return Err(not_owner(format!(
                    "{} {name}",
                    routine_word(existing.prokind)
                )));
            }
        }
        Ok(())
    }

    fn alter_table(&self, s: &ast::AlterTableStmt) -> Result<(), DdlError> {
        let Some(rv) = s.relation.as_deref() else {
            return Ok(());
        };
        let Some(relid) = self.existing(rv) else {
            return Ok(());
        };
        // RangeVarCallbackForAlterRelation: the relation's owner.
        self.own_relation(relid, &rv.relname)?;
        for cmd in s.cmds.iter().flatten() {
            let Node::AlterTableCmd(cmd) = cmd else {
                continue;
            };
            match cmd.subtype {
                AlterTableType::AtAddConstraint => {
                    if let Some(Node::Constraint(c)) = cmd.def.as_ref() {
                        let own = (rv.schemaname.as_str(), rv.relname.as_str());
                        self.references(c, Some(own))?;
                    }
                }
                AlterTableType::AtAddColumn => {
                    if let Some(Node::ColumnDef(cd)) = cmd.def.as_ref() {
                        if let Some(tn) = cd.type_name.as_deref()
                            && let Some(oid) = super::util::resolve_type_name(tn, self.db)
                        {
                            self.use_type(oid)?;
                        }
                        for c in cd.constraints.iter().flatten() {
                            if let Node::Constraint(c) = c {
                                let own = (rv.schemaname.as_str(), rv.relname.as_str());
                                self.references(c, Some(own))?;
                            }
                        }
                    }
                }
                AlterTableType::AtChangeOwner => {
                    if let Some(spec) = cmd.newowner.as_deref()
                        && let Some(c) = self.relation(relid)
                    {
                        self.give_away(
                            AclObject::Relation(relid),
                            spec,
                            Some(c.relnamespace),
                            &format!("{} {}", relkind_word(c.relkind), c.relname),
                        )?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// `OWNER TO`: once the object's owner (the caller checked how), the
    /// role must be able to become the new owner (`check_can_set_role`),
    /// which needs CREATE on the object's schema (`namespace`) — on the
    /// database for a schema. Nothing when the owner doesn't change.
    fn give_away(
        &self,
        object: AclObject,
        spec: &ast::RoleSpec,
        namespace: Option<PgNamespaceOid>,
        what: &str,
    ) -> Result<(), DdlError> {
        let Some(new_owner) = self.role_spec(spec) else {
            return Ok(());
        };
        let old_owner = self.db.owner_of(object);
        if self.db.roles.same_real_role(old_owner, new_owner) {
            return Ok(());
        }
        if !self.owns(object) {
            return Err(not_owner(what));
        }
        if !self.db.roles.can_set_role(self.role, new_owner) {
            return Err(DdlError::UnsupportedDdl(format!(
                "must be able to SET ROLE \"{}\"",
                self.db.roles.display_name(new_owner)
            )));
        }
        let new_name = self.db.roles.name(new_owner).to_owned();
        match namespace {
            Some(ns) => {
                // A role the catalog doesn't know may hold anything.
                if self
                    .db
                    .roles
                    .get(new_owner)
                    .is_some_and(|r| r.created || r.superuser)
                    && !self.schema_allows(&new_name, ns, Privilege::Create)
                {
                    return Err(denied(format!("schema {}", self.schema_name(ns))));
                }
            }
            None => {
                // AlterSchemaOwner_internal: CREATE on the database, of the
                // current role.
                if !super::acl::has_database_create(self.db, self.role) {
                    return Err(denied(format!("database {}", database_name(self.db))));
                }
            }
        }
        Ok(())
    }

    /// The role a role spec names, without interning a new one: `None`
    /// for one the statement reports (PUBLIC, a missing role) or one the
    /// catalog doesn't know (it may be anyone).
    fn role_spec(&self, spec: &ast::RoleSpec) -> Option<RoleId> {
        use ast::RoleSpecType;
        match spec.roletype {
            RoleSpecType::RolespecCstring => self.db.roles.id(&spec.rolename),
            RoleSpecType::RolespecCurrentRole | RoleSpecType::RolespecCurrentUser => {
                Some(self.role)
            }
            RoleSpecType::RolespecSessionUser => Some(
                self.db
                    .session_identity
                    .session_user()
                    .and_then(|name| self.db.roles.id(name))
                    .unwrap_or(self.db.roles.migrations_user),
            ),
            RoleSpecType::RolespecPublic => None,
        }
    }

    fn rename(&self, s: &ast::RenameStmt) -> Result<(), DdlError> {
        match s.rename_type {
            ObjectType::ObjectTable
            | ObjectType::ObjectView
            | ObjectType::ObjectMatview
            | ObjectType::ObjectSequence
            | ObjectType::ObjectForeignTable
            | ObjectType::ObjectIndex
            | ObjectType::ObjectColumn
            | ObjectType::ObjectTabconstraint => {
                let Some(rv) = s.relation.as_deref() else {
                    return Ok(());
                };
                let Some(relid) = self.existing(rv) else {
                    return Ok(());
                };
                // renameatt_check / RenameConstraint report the relation's
                // own name, RangeVarCallbackForAlterRelation the name as
                // written.
                let name = match s.rename_type {
                    ObjectType::ObjectColumn | ObjectType::ObjectTabconstraint => {
                        relation_name(self.db, relid, &rv.relname)
                    }
                    _ => rv.relname.clone(),
                };
                self.own_relation(relid, &name)
            }
            ObjectType::ObjectSchema => match self.db.namespace_oid(&s.subname) {
                Some(ns) if !self.owns(AclObject::Schema(ns)) => {
                    Err(not_owner(format!("schema {}", s.subname)))
                }
                // RenameSchema: CREATE on the database too.
                Some(_) if !super::acl::has_database_create(self.db, self.role) => {
                    Err(denied(format!("database {}", database_name(self.db))))
                }
                _ => Ok(()),
            },
            _ => Ok(()),
        }
    }

    /// DROP: the object's owner, or the owner of its schema
    /// (`RangeVarCallbackForDropRelation`, `RemoveObjects`).
    fn drop(&self, s: &ast::DropStmt) -> Result<(), DdlError> {
        for object in s.objects.iter().flatten() {
            match s.remove_type {
                t if is_relation_kind(t) => {
                    let Node::List(l) = object else { continue };
                    let Some(rv) = range_var_of(&l.items) else {
                        continue;
                    };
                    let Some(relid) = self.existing(&rv) else {
                        continue;
                    };
                    let schema_owner = self
                        .relation(relid)
                        .is_some_and(|c| self.owns(AclObject::Schema(c.relnamespace)));
                    if !schema_owner {
                        self.own_relation(relid, &rv.relname)?;
                    }
                }
                ObjectType::ObjectType | ObjectType::ObjectDomain => {
                    let Node::TypeName(tn) = object else { continue };
                    let Some(oid) = super::util::resolve_type_name(tn, self.db) else {
                        continue;
                    };
                    let Some(t) = self.db.get_type(oid) else {
                        continue;
                    };
                    if !self.owns(AclObject::Schema(t.typnamespace))
                        && !self.owns(AclObject::Type(oid))
                    {
                        return Err(not_owner(format!(
                            "type {}",
                            super::util::format_type_for_message(self.db, oid)
                        )));
                    }
                }
                ObjectType::ObjectFunction
                | ObjectType::ObjectProcedure
                | ObjectType::ObjectRoutine
                | ObjectType::ObjectAggregate => {
                    let Node::ObjectWithArgs(owa) = object else {
                        continue;
                    };
                    let Ok(Some(oid)) =
                        super::functions::lookup_func_with_args(self.db, s.remove_type, owa, true)
                    else {
                        continue;
                    };
                    let Some(p) = self.db.pg_proc.get(&oid) else {
                        continue;
                    };
                    if !self.owns(AclObject::Schema(p.pronamespace))
                        && !self.owns(AclObject::Function(oid))
                    {
                        return Err(not_owner(format!(
                            "{} {}",
                            object_word(s.remove_type, p.prokind),
                            super::functions::object_name(owa).join(".")
                        )));
                    }
                }
                ObjectType::ObjectSchema => {
                    let Some(name) = super::util::node_string(Some(object)) else {
                        continue;
                    };
                    if let Some(ns) = self.db.namespace_oid(name)
                        && !self.owns(AclObject::Schema(ns))
                    {
                        return Err(not_owner(format!("schema {name}")));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// COMMENT ON: the object's owner (`check_object_ownership`).
    fn comment(&self, s: &ast::CommentStmt) -> Result<(), DdlError> {
        let Some(object) = s.object.as_ref() else {
            return Ok(());
        };
        match s.objtype {
            t if is_relation_kind(t) || t == ObjectType::ObjectColumn => {
                let Node::List(l) = object else {
                    return Ok(());
                };
                let items = if t == ObjectType::ObjectColumn {
                    &l.items[..l.items.len().saturating_sub(1)]
                } else {
                    &l.items[..]
                };
                let Some(rv) = range_var_of(items) else {
                    return Ok(());
                };
                let Some(relid) = self.existing(&rv) else {
                    return Ok(());
                };
                if self.owns(AclObject::Relation(relid)) {
                    return Ok(());
                }
                let name = relation_name(self.db, relid, &rv.relname);
                Err(not_owner(if t == ObjectType::ObjectColumn {
                    format!("relation {name}")
                } else {
                    format!(
                        "{} {name}",
                        self.relation(relid)
                            .map_or("table", |c| relkind_word(c.relkind))
                    )
                }))
            }
            ObjectType::ObjectType | ObjectType::ObjectDomain => {
                let Node::TypeName(tn) = object else {
                    return Ok(());
                };
                match super::util::resolve_type_name(tn, self.db) {
                    Some(oid) if !self.owns(AclObject::Type(oid)) => Err(not_owner(format!(
                        "type {}",
                        super::util::format_type_for_message(self.db, oid)
                    ))),
                    _ => Ok(()),
                }
            }
            ObjectType::ObjectFunction
            | ObjectType::ObjectProcedure
            | ObjectType::ObjectRoutine => {
                let Node::ObjectWithArgs(owa) = object else {
                    return Ok(());
                };
                let Ok(Some(oid)) =
                    super::functions::lookup_func_with_args(self.db, s.objtype, owa, true)
                else {
                    return Ok(());
                };
                match self.db.pg_proc.get(&oid) {
                    Some(p) if !self.owns(AclObject::Function(oid)) => Err(not_owner(format!(
                        "{} {}",
                        object_word(s.objtype, p.prokind),
                        super::functions::object_name(owa).join(".")
                    ))),
                    _ => Ok(()),
                }
            }
            ObjectType::ObjectSchema => {
                let Some(name) = super::util::node_string(Some(object)) else {
                    return Ok(());
                };
                match self.db.namespace_oid(name) {
                    Some(ns) if !self.owns(AclObject::Schema(ns)) => {
                        Err(not_owner(format!("schema {name}")))
                    }
                    _ => Ok(()),
                }
            }
            _ => Ok(()),
        }
    }

    /// ALTER … OWNER TO for schemas, types and routines
    /// (`AlterSchemaOwner_internal`, `AlterTypeOwner`,
    /// `AlterObjectOwner_internal`).
    fn alter_owner(&self, s: &ast::AlterOwnerStmt) -> Result<(), DdlError> {
        let Some(spec) = s.newowner.as_deref() else {
            return Ok(());
        };
        let Some(object) = s.object.as_ref() else {
            return Ok(());
        };
        match s.object_type {
            ObjectType::ObjectSchema => {
                let Some(name) = super::util::node_string(Some(object)) else {
                    return Ok(());
                };
                let Some(ns) = self.db.namespace_oid(name) else {
                    return Ok(());
                };
                self.give_away(AclObject::Schema(ns), spec, None, &format!("schema {name}"))
            }
            ObjectType::ObjectType | ObjectType::ObjectDomain => {
                let names: Vec<Option<Node>> = match object {
                    Node::List(l) => l.items.clone(),
                    Node::TypeName(tn) => tn.names.clone(),
                    _ => return Ok(()),
                };
                let tn = ast::TypeName {
                    names,
                    typemod: -1,
                    location: -1,
                    ..Default::default()
                };
                let Some(oid) = super::util::resolve_type_name(&tn, self.db) else {
                    return Ok(());
                };
                let Some(t) = self.db.get_type(oid) else {
                    return Ok(());
                };
                self.give_away(
                    AclObject::Type(oid),
                    spec,
                    Some(t.typnamespace),
                    &format!(
                        "type {}",
                        super::util::format_type_for_message(self.db, oid)
                    ),
                )
            }
            ObjectType::ObjectFunction
            | ObjectType::ObjectProcedure
            | ObjectType::ObjectRoutine => {
                let Node::ObjectWithArgs(owa) = object else {
                    return Ok(());
                };
                let Ok(Some(oid)) =
                    super::functions::lookup_func_with_args(self.db, s.object_type, owa, true)
                else {
                    return Ok(());
                };
                let Some(p) = self.db.pg_proc.get(&oid) else {
                    return Ok(());
                };
                self.give_away(
                    AclObject::Function(oid),
                    spec,
                    Some(p.pronamespace),
                    &format!("{} {}", routine_word(p.prokind), p.proname),
                )
            }
            _ => Ok(()),
        }
    }

    /// CREATE SCHEMA: CREATE on the database, and being able to become the
    /// owner named by AUTHORIZATION (`CreateSchemaCommand`).
    fn create_schema(&self, s: &ast::CreateSchemaStmt) -> Result<(), DdlError> {
        if !super::acl::has_database_create(self.db, self.role) {
            return Err(denied(format!("database {}", database_name(self.db))));
        }
        if let Some(spec) = s.authrole.as_deref()
            && let Some(owner) = self.role_spec(spec)
            && !self.db.roles.can_set_role(self.role, owner)
        {
            return Err(DdlError::UnsupportedDdl(format!(
                "must be able to SET ROLE \"{}\"",
                self.db.roles.display_name(owner)
            )));
        }
        Ok(())
    }
}

/// The names of relation `relid`'s primary key columns, in key order.
fn primary_key_columns(db: &PgCatalog, relid: PgClassOid) -> Option<Vec<String>> {
    let attrs = db.attributes_of(relid);
    db.pg_constraint
        .values()
        .find(|c| c.conrelid == relid && c.contype == crate::pg_catalog::ConType::PrimaryKey)
        .map(|c| {
            c.conkey
                .iter()
                .filter_map(|k| attrs.iter().find(|a| a.attnum == *k))
                .map(|a| a.attname.clone())
                .collect()
        })
}

/// The schema of a qualified name (`[schema, name]`, `[db, schema, name]`).
fn qualifier(names: Vec<String>) -> Option<String> {
    (names.len() >= 2).then(|| names[names.len() - 2].clone())
}

/// Object kinds a DROP or COMMENT names as a relation.
fn is_relation_kind(t: ObjectType) -> bool {
    matches!(
        t,
        ObjectType::ObjectTable
            | ObjectType::ObjectView
            | ObjectType::ObjectMatview
            | ObjectType::ObjectSequence
            | ObjectType::ObjectForeignTable
            | ObjectType::ObjectIndex
    )
}

/// A qualified name (`[schema.]name`) as a RangeVar.
fn range_var_of(items: &[Option<Node>]) -> Option<ast::RangeVar> {
    let names: Vec<&str> = items
        .iter()
        .filter_map(|n| super::util::node_string(n.as_ref()))
        .collect();
    let (schema, name) = match names.as_slice() {
        [name] => ("", *name),
        [.., schema, name] => (*schema, *name),
        [] => return None,
    };
    Some(ast::RangeVar {
        schemaname: schema.to_owned(),
        relname: name.to_owned(),
        inh: true,
        relpersistence: "p".to_owned(),
        location: -1,
        ..Default::default()
    })
}

/// A relation's own name (the RangeVar's when it is gone).
fn relation_name(db: &PgCatalog, relid: PgClassOid, written: &str) -> String {
    db.pg_class
        .get(&relid)
        .map_or_else(|| written.to_owned(), |c| c.relname.clone())
}

fn routine_word(kind: crate::pg_catalog::ProKind) -> &'static str {
    match kind {
        crate::pg_catalog::ProKind::Procedure => "procedure",
        crate::pg_catalog::ProKind::Aggregate => "aggregate",
        _ => "function",
    }
}

/// The word `aclcheck_error` says for a routine a statement names as
/// `objtype` (a ROUTINE stays one).
fn object_word(objtype: ObjectType, kind: crate::pg_catalog::ProKind) -> &'static str {
    match objtype {
        ObjectType::ObjectRoutine => "routine",
        ObjectType::ObjectAggregate => "aggregate",
        ObjectType::ObjectProcedure => "procedure",
        _ => routine_word(kind),
    }
}

/// The current database's name, as `get_database_name` gives it: known
/// when the caller told it ([`PgCatalog::set_database_name`]).
fn database_name(db: &PgCatalog) -> String {
    db.session_identity
        .database()
        .unwrap_or("(the current database)")
        .to_owned()
}
