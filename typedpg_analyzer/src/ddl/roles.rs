//! Roles, role attributes and role memberships (`pg_authid` /
//! `pg_auth_members`), and the role[l] statements that manage the cluster's
//! roles: CREATE / ALTER / DROP ROLE, GRANT / REVOKE role, REASSIGN OWNED and
//! DROP OWNED.
//!
//! Roles live in the cluster, not in a database, and the migrations see only
//! the ones they created themselves: a role they merely name (the connecting
//! user included) is *external* — assumed to exist, but the analyzer knows
//! only the grants made to and among it. The migrations' own user is
//! [`PgCatalog::MIGRATIONS_USER`]: it is unknown, but it is taken to be
//! superuser-equivalent and to own every object created while no `SET ROLE` /
//! `SET SESSION AUTHORIZATION` is active (like PostgreSQL's connecting user
//! when the migration runner is the database owner).

use typedpg_pg_query::ast::{self, DropBehavior, RoleSpecType, RoleStmtType};

use super::DdlError;
use super::session;
use crate::pg_catalog::PgCatalog;

/// A role's identity inside the catalog. Stable across `ALTER ROLE ...
/// RENAME TO`: every owner and ACL entry names the role by this id, so a
/// rename never has to rewrite grants (PG keys them by `pg_authid.oid` too).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct RoleId(pub(crate) u32);

/// What the catalog knows of one role (`pg_authid`'s attributes the
/// privilege model reads).
#[derive(Debug, Clone)]
pub(crate) struct Role {
    pub(crate) name: String,
    pub(crate) superuser: bool,
    /// `rolinherit`: the default `INHERIT` option of the memberships this
    /// role is granted (`InitGrantRoleOptions` + `AddRoleMems`).
    pub(crate) inherit: bool,
    pub(crate) createrole: bool,
    pub(crate) createdb: bool,
    pub(crate) bypassrls: bool,
    pub(crate) login: bool,
    /// Created by a migration's `CREATE ROLE` (or `CREATE USER` / `CREATE
    /// GROUP`), so the analyzer can simulate it.
    pub(crate) created: bool,
}

/// One `pg_auth_members` row: `member` is a member of `role`, granted by
/// `grantor`. The options are PG 16+'s per-membership flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Membership {
    pub(crate) role: RoleId,
    pub(crate) member: RoleId,
    pub(crate) grantor: RoleId,
    pub(crate) admin: bool,
    pub(crate) inherit: bool,
    pub(crate) set: bool,
    /// The row's OID in PG (`pg_auth_members.oid`), which orders the
    /// dependency DETAIL lines a DROP ROLE reports. The built-in grants get
    /// the lowest values; every new grant takes the next catalog OID.
    pub(crate) oid: u32,
}

/// The cluster's roles and memberships.
#[derive(Debug, Clone)]
pub(crate) struct Roles {
    roles: std::collections::HashMap<RoleId, Role>,
    by_name: std::collections::HashMap<String, RoleId>,
    pub(crate) memberships: Vec<Membership>,
    next_id: u32,
    /// The bootstrap superuser (`postgres` in the seed image), owner of the
    /// built-in objects.
    pub(crate) bootstrap: RoleId,
    /// The role the migrations run as while no `SET ROLE` is active.
    pub(crate) migrations_user: RoleId,
    /// `ALTER ROLE r SET search_path`: the setting each role's new sessions
    /// start with (pg_db_role_setting), by role.
    pub(crate) search_paths: std::collections::HashMap<RoleId, Vec<String>>,
    /// `ALTER ROLE ALL SET search_path`: every role's.
    pub(crate) all_search_path: Option<Vec<String>>,
    /// `ALTER ROLE r SET row_security`: the `row_security` GUC each role's
    /// new sessions start with (pg_db_role_setting). A session of the role
    /// with it off makes PG reject every statement row security would
    /// affect (`check_enable_rls`) instead of filtering its rows.
    pub(crate) row_security: std::collections::HashMap<RoleId, bool>,
    /// `ALTER ROLE ALL SET row_security`: every role's.
    pub(crate) all_row_security: Option<bool>,
    /// What code the analyzer couldn't follow may have changed (see
    /// [`super::uncertain`]).
    pub(crate) uncertain: super::uncertain::Uncertain,
}

impl Default for Roles {
    fn default() -> Self {
        Self::new()
    }
}

impl Roles {
    pub(crate) fn new() -> Self {
        let mut roles = Roles {
            roles: std::collections::HashMap::new(),
            by_name: std::collections::HashMap::new(),
            memberships: Vec::new(),
            next_id: 1,
            bootstrap: RoleId(0),
            migrations_user: RoleId(0),
            search_paths: std::collections::HashMap::new(),
            all_search_path: None,
            row_security: std::collections::HashMap::new(),
            all_row_security: None,
            uncertain: super::uncertain::Uncertain::default(),
        };
        // Built-in roles first: their ids stay small and stable.
        roles.bootstrap = roles.add("postgres", false, true);
        roles.migrations_user = roles.add(PgCatalog::MIGRATIONS_USER, false, true);
        for name in session::PREDEFINED_ROLES {
            roles.add(name, false, false);
        }
        // pg_monitor's fixed memberships (pg_authid.dat / system_views.sql).
        let monitor = roles.id("pg_monitor").expect("pg_monitor is predefined");
        for (oid, granted) in [
            "pg_read_all_settings",
            "pg_read_all_stats",
            "pg_stat_scan_tables",
        ]
        .into_iter()
        .enumerate()
        {
            let role = roles.id(granted).expect("predefined role");
            roles.memberships.push(Membership {
                role,
                member: monitor,
                grantor: roles.bootstrap,
                admin: false,
                inherit: true,
                set: true,
                // Initdb assigns these rows OIDs below the user range; the
                // only thing that matters is that they sort first.
                oid: oid as u32,
            });
        }
        roles
    }

    fn add(&mut self, name: &str, created: bool, superuser: bool) -> RoleId {
        if let Some(&id) = self.by_name.get(name) {
            return id;
        }
        let id = RoleId(self.next_id);
        self.next_id += 1;
        self.roles.insert(
            id,
            Role {
                name: name.to_owned(),
                superuser,
                inherit: true,
                createrole: false,
                createdb: false,
                bypassrls: false,
                login: false,
                created,
            },
        );
        self.by_name.insert(name.to_owned(), id);
        id
    }

    /// The id the real cluster uses for a model role: the migrations' user
    /// and the bootstrap superuser are the same real role (see
    /// [`PgCatalog::MIGRATIONS_USER`]), so every recorded grantor, owner,
    /// default-privilege key and identity comparison uses the bootstrap's
    /// id. Created and other predefined roles are their own.
    pub(crate) fn canonical(&self, id: RoleId) -> RoleId {
        if self.same_real_role(id, self.bootstrap) {
            self.bootstrap
        } else {
            id
        }
    }

    /// The name PG prints for a model role: the migrations' user prints as
    /// the bootstrap superuser, the role it really is.
    pub(crate) fn display_name(&self, id: RoleId) -> &str {
        self.name(self.canonical(id))
    }

    pub(crate) fn get(&self, id: RoleId) -> Option<&Role> {
        self.roles.get(&id)
    }

    /// The id of the role named `name`: a role the catalog knows, or an
    /// external one it interns now.
    pub(crate) fn id_or_intern(&mut self, name: &str) -> RoleId {
        if let Some(&id) = self.by_name.get(name) {
            return id;
        }
        self.add(name, false, false)
    }

    pub(crate) fn id(&self, name: &str) -> Option<RoleId> {
        self.by_name.get(name).copied()
    }

    pub(crate) fn name(&self, id: RoleId) -> &str {
        self.roles
            .get(&id)
            .map_or("<unknown role>", |r| r.name.as_str())
    }

    pub(crate) fn is_superuser(&self, id: RoleId) -> bool {
        self.roles.get(&id).is_some_and(|r| r.superuser)
    }

    pub(crate) fn exists(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// Whether `name` is a role with `rolcanlogin` (`pg_user` / `pg_shadow`
    /// list only those; `pg_roles` lists every role).
    pub(crate) fn exists_with_login(&self, name: &str) -> bool {
        self.by_name
            .get(name)
            .is_some_and(|id| self.roles.get(id).is_some_and(|r| r.login))
    }

    /// The roles the migrations created, in creation order: the ones the
    /// `pg_sanity` oracle compares and drops at teardown.
    #[cfg(feature = "pg_sanity")]
    pub(crate) fn created_names(&self) -> Vec<String> {
        let mut rows: Vec<(RoleId, &str)> = self
            .roles
            .iter()
            .filter(|(_, r)| r.created)
            .map(|(&id, r)| (id, r.name.as_str()))
            .collect();
        rows.sort_by_key(|(id, _)| *id);
        rows.into_iter().map(|(_, name)| name.to_owned()).collect()
    }

    fn rename(&mut self, id: RoleId, new_name: String) {
        if let Some(role) = self.roles.get_mut(&id) {
            self.by_name.remove(&role.name);
            role.name = new_name.clone();
            self.by_name.insert(new_name, id);
        }
    }

    fn remove(&mut self, id: RoleId) {
        if let Some(role) = self.roles.remove(&id) {
            self.by_name.remove(&role.name);
        }
        self.search_paths.remove(&id);
        self.row_security.remove(&id);
        self.memberships
            .retain(|m| m.role != id && m.member != id && m.grantor != id);
    }

    /// `has_privs_of_role` (acl.c): the role itself, a superuser, or a role
    /// reached through memberships whose `INHERIT` option is on. The
    /// member's own `INHERIT` attribute only chooses that option's default
    /// when a membership is granted, so it isn't consulted here.
    pub(crate) fn has_privs_of_role(&self, member: RoleId, role: RoleId) -> bool {
        if self.same_real_role(member, role) || self.is_superuser(member) {
            return true;
        }
        // `roles_is_member_of`'s inheritable-membership closure (which also
        // gives the database owner pg_database_owner's implicit membership).
        self.roles_with_privs_of(member)
            .into_iter()
            .any(|other| self.same_real_role(other, role))
    }

    /// Whether two role ids name the same cluster role: the migrations' user
    /// and the bootstrap superuser are the same real role (the connecting
    /// user), which the model keeps apart because its own name is unknown.
    pub(crate) fn same_real_role(&self, left: RoleId, right: RoleId) -> bool {
        left == right
            || (left == self.migrations_user && right == self.bootstrap)
            || (left == self.bootstrap && right == self.migrations_user)
    }

    /// `roles_is_member_of(role, ROLERECURSE_PRIVS)`: the role itself and
    /// every role it has the privileges of, in breadth-first order (closer
    /// relationships first), as `select_best_admin` / `select_best_grantor`
    /// search them. Like PG, the database owner is implicitly a member of
    /// `pg_database_owner`; the model's database — the oracle's scratch
    /// database — belongs to the migrations' user (the bootstrap superuser).
    pub(crate) fn roles_with_privs_of(&self, member: RoleId) -> Vec<RoleId> {
        let member = self.canonical(member);
        let mut found = vec![member];
        let database_owner = self.canonical(self.migrations_user);
        let database_owner_role = self.id("pg_database_owner");
        let mut next = 0;
        while next < found.len() {
            let current = found[next];
            next += 1;
            let mut added: Vec<RoleId> = self
                .memberships
                .iter()
                .filter(|m| m.member == current && m.inherit && !self.may_be_lost(m))
                .map(|m| m.role)
                .collect();
            added.extend(self.maybe_gained_roles_of(current));
            added.sort();
            added.dedup();
            for role in added {
                if !found.contains(&role) {
                    found.push(role);
                }
            }
            // `roles_is_member_of` implements pg_database_owner's implicit
            // membership right after the database owner's own grants.
            if current == database_owner
                && let Some(database_owner_role) = database_owner_role
                && !found.contains(&database_owner_role)
            {
                found.push(database_owner_role);
            }
        }
        found
    }

    /// `member_can_set_role`: a superuser may become anyone; anyone else
    /// the roles reachable through memberships granted WITH SET.
    pub(crate) fn can_set_role(&self, member: RoleId, role: RoleId) -> bool {
        if self.is_superuser(member) || self.same_real_role(member, role) {
            return true;
        }
        let mut seen = vec![self.canonical(member)];
        let mut next = 0;
        while next < seen.len() {
            let current = seen[next];
            next += 1;
            for m in &self.memberships {
                if self.same_real_role(m.member, current) && m.set && !self.may_be_lost(m) {
                    if self.same_real_role(m.role, role) {
                        return true;
                    }
                    if !seen.contains(&m.role) {
                        seen.push(m.role);
                    }
                }
            }
            for gained in self.maybe_gained_roles_of(current) {
                if self.same_real_role(gained, role) {
                    return true;
                }
                if !seen.contains(&gained) {
                    seen.push(gained);
                }
            }
        }
        false
    }

    /// Whether membership `m` may be gone, when checks read the lower bound
    /// of an uncertain catalog (see [`super::uncertain`]).
    fn may_be_lost(&self, m: &Membership) -> bool {
        super::uncertain::bound() == super::uncertain::Bound::Lower
            && self
                .uncertain
                .membership_may_be_lost(self.name(m.role), self.name(m.member))
    }

    /// The roles `member` may be a member of without the catalog recording
    /// it (with every option), when checks read the upper bound of an
    /// uncertain catalog (see [`super::uncertain`]).
    fn maybe_gained_roles_of(&self, member: RoleId) -> Vec<RoleId> {
        if super::uncertain::bound() != super::uncertain::Bound::Upper
            || (self.uncertain.gained_memberships.is_empty() && !self.uncertain.catalog)
        {
            return Vec::new();
        }
        let member_name = self.name(member);
        let mut roles: Vec<RoleId> = self
            .roles
            .iter()
            .filter(|&(&id, role)| {
                id != member
                    && self
                        .uncertain
                        .membership_may_be_gained(&role.name, member_name)
            })
            .map(|(&id, _)| id)
            .collect();
        roles.sort();
        roles
    }

    /// `select_best_admin` (acl.c): the role closest to `member` (itself
    /// first) that directly holds ADMIN OPTION on `role`, ignoring
    /// superuserness. `None` when no such role exists.
    pub(crate) fn select_best_admin(&self, member: RoleId, role: RoleId) -> Option<RoleId> {
        if self.same_real_role(member, role) {
            return None;
        }
        self.roles_with_privs_of(member).into_iter().find(|&other| {
            self.memberships.iter().any(|m| {
                m.role == role
                    && self.same_real_role(m.member, other)
                    && m.admin
                    && !self.may_be_lost(m)
            }) || self.maybe_gained_roles_of(other).contains(&role)
        })
    }

    /// `is_member_of_role_nosuper`: membership regardless of the inherit
    /// option (the loop check of `AddRoleMems`).
    fn is_member_nosuper(&self, member: RoleId, role: RoleId) -> bool {
        if member == role {
            return true;
        }
        let mut seen = vec![member];
        let mut queue = vec![member];
        while let Some(current) = queue.pop() {
            for m in &self.memberships {
                if m.member == current && !seen.contains(&m.role) {
                    if m.role == role {
                        return true;
                    }
                    seen.push(m.role);
                    queue.push(m.role);
                }
            }
        }
        false
    }
}

/// The role `spec` names (`get_rolespec_oid`). `None` for `PUBLIC`, which
/// never is a role. `CURRENT_USER` / `CURRENT_ROLE` / `SESSION_USER` are the
/// migrations' user while the session leaves them unnamed.
fn role_spec_name(interp: &PgCatalog, spec: &typedpg_pg_query::ast::RoleSpec) -> Option<String> {
    match spec.roletype {
        RoleSpecType::RolespecCstring => Some(spec.rolename.clone()),
        RoleSpecType::RolespecCurrentRole | RoleSpecType::RolespecCurrentUser => Some(
            interp
                .session_identity
                .current_user()
                .unwrap_or(PgCatalog::MIGRATIONS_USER)
                .to_owned(),
        ),
        RoleSpecType::RolespecSessionUser => Some(
            interp
                .session_identity
                .session_user()
                .unwrap_or(PgCatalog::MIGRATIONS_USER)
                .to_owned(),
        ),
        RoleSpecType::RolespecPublic => None,
    }
}

/// `get_rolespec_oid(..., false)` for a role a statement needs to exist.
pub(crate) fn resolve_role_spec(
    interp: &mut PgCatalog,
    spec: &typedpg_pg_query::ast::RoleSpec,
) -> Result<RoleId, DdlError> {
    let Some(name) = role_spec_name(interp, spec) else {
        // PUBLIC: `role "public" does not exist`.
        let shown = if spec.roletype == RoleSpecType::RolespecPublic {
            "public"
        } else {
            &spec.rolename
        };
        return Err(DdlError::TypeNotFound(format!(
            "role \"{shown}\" does not exist"
        )));
    };
    if !session::role_may_exist(&name) && name != PgCatalog::MIGRATIONS_USER {
        return Err(DdlError::TypeNotFound(format!(
            "role \"{name}\" does not exist"
        )));
    }
    if name == PgCatalog::MIGRATIONS_USER {
        return Ok(interp.roles.migrations_user);
    }
    Ok(interp.roles.id_or_intern(&name))
}

/// `ALTER ROLE { r | ALL } [IN DATABASE d] SET / RESET ...`
/// (AlterRoleSet): a setting for the role's new sessions. As for `ALTER
/// DATABASE ... SET`, any database is taken as the application's. Two
/// settings matter: `search_path`, which decides what the role's statements
/// resolve their unqualified names to, and `row_security`, whose `off` makes
/// every statement row security affects fail (`check_enable_rls`).
pub fn alter_role_set(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::ast::AlterRoleSetStmt,
) -> Result<(), DdlError> {
    use typedpg_pg_query::ast::VariableSetKind;
    let role = match &stmt.role {
        Some(spec) => Some(resolve_role_spec(interp, spec)?),
        None => None,
    };
    let Some(set) = stmt.setstmt.as_ref() else {
        return Ok(());
    };
    let kind = set.kind;
    let is_search_path = set.name.eq_ignore_ascii_case("search_path");
    let is_row_security = set.name.eq_ignore_ascii_case("row_security");
    if kind == VariableSetKind::VarResetAll {
        // RESET ALL drops every setting `pg_db_role_setting` keeps.
        match role {
            Some(id) => {
                interp.roles.search_paths.remove(&id);
                interp.roles.row_security.remove(&id);
            }
            None => {
                interp.roles.all_search_path = None;
                interp.roles.all_row_security = None;
            }
        }
        return Ok(());
    }
    // The parameter must exist and be settable, and a new value valid, as
    // for SET.
    let null = kind == VariableSetKind::VarSetValue && super::guc::null_arg(&set.name, &set.args)?;
    if let Some(setting) = super::guc::check_settable(interp, &set.name)?.cloned()
        && kind == VariableSetKind::VarSetValue
        && !null
        && let [arg] = set.args.as_slice()
        && let Some(value) = session::const_arg_string(arg.as_ref())
    {
        super::guc::check_value(interp, &setting, &value)?;
    }
    if is_row_security {
        let value = match kind {
            VariableSetKind::VarSetValue => match set.args.as_slice() {
                [arg] => session::const_arg_string(arg.as_ref())
                    .and_then(|v| super::reloptions::parse_bool(&v)),
                _ => None,
            },
            // FROM CURRENT: the migrations' session's value now.
            VariableSetKind::VarSetCurrent => Some(interp.row_security_guc.get()),
            _ => None,
        };
        match role {
            Some(id) => match value {
                Some(v) => {
                    interp.roles.row_security.insert(id, v);
                }
                None => {
                    interp.roles.row_security.remove(&id);
                }
            },
            None => interp.roles.all_row_security = value,
        }
        return Ok(());
    }
    if !is_search_path {
        return Ok(());
    }
    let value = match kind {
        VariableSetKind::VarSetValue => match session::search_path_args(set) {
            Some(names) => Some(names),
            None => return Ok(()),
        },
        // FROM CURRENT: the migrations' session's value now.
        VariableSetKind::VarSetCurrent => Some(interp.search_path_guc.effective().to_vec()),
        _ => None,
    };
    match role {
        Some(id) => match value {
            Some(v) => {
                interp.roles.search_paths.insert(id, v);
            }
            None => {
                interp.roles.search_paths.remove(&id);
            }
        },
        None => interp.roles.all_search_path = value,
    }
    Ok(())
}

/// The role the session acts as: `current_user`, or the migrations' user.
pub(crate) fn current_role(interp: &mut PgCatalog) -> RoleId {
    match interp.session_identity.current_user() {
        Some(name) => {
            let name = name.to_owned();
            interp.roles.id_or_intern(&name)
        }
        None => interp.roles.migrations_user,
    }
}

/// A role name a `CREATE ROLE`-family statement may take: PG's
/// `IsReservedName` refuses `public`, `none` and the `pg_` prefix.
fn check_new_role_name(name: &str) -> Result<(), DdlError> {
    check_role_name_chars(name)?;
    if name == "public" || name == "none" || name.starts_with("pg_") {
        return Err(DdlError::Parse(format!("role name \"{name}\" is reserved")));
    }
    Ok(())
}

/// CreateRole / RenameRole (PostgreSQL 19): no newline or carriage return
/// in a role name.
fn check_role_name_chars(name: &str) -> Result<(), DdlError> {
    if name.contains(['\n', '\r']) {
        return Err(DdlError::Parse(format!(
            "role name \"{name}\" contains a newline or carriage return character"
        )));
    }
    Ok(())
}

/// The boolean of a role option (`defGetBoolean`, but role options carry
/// Boolean nodes directly).
fn option_bool(_interp: &PgCatalog, de: &typedpg_pg_query::ast::DefElem) -> Result<bool, DdlError> {
    match de.arg.as_ref() {
        None => Ok(true),
        Some(ast::Node::Boolean(b)) => Ok(b.boolval),
        Some(ast::Node::Integer(i)) => Ok(i.ival != 0),
        Some(ast::Node::String(s)) => match s.sval.to_ascii_lowercase().as_str() {
            "true" | "on" | "1" => Ok(true),
            "false" | "off" | "0" => Ok(false),
            _ => Err(DdlError::Parse(format!(
                "{} requires a Boolean value",
                de.defname
            ))),
        },
        Some(_) => Err(DdlError::Parse(format!(
            "{} requires a Boolean value",
            de.defname
        ))),
    }
}

/// The role specs of a `List` option (`rolemembers`, `addroleto`,
/// `adminmembers`).
fn role_list(option: &typedpg_pg_query::ast::DefElem) -> Vec<&typedpg_pg_query::ast::RoleSpec> {
    let Some(ast::Node::List(list)) = option.arg.as_ref() else {
        return Vec::new();
    };
    list.items
        .iter()
        .filter_map(|n| match n.as_ref() {
            Some(ast::Node::RoleSpec(rs)) => Some(&**rs),
            _ => None,
        })
        .collect()
}

/// Add one membership (`AddRoleMems`).
fn add_membership(
    interp: &mut PgCatalog,
    role: RoleId,
    member: RoleId,
    grantor: RoleId,
    admin: Option<bool>,
    inherit: Option<bool>,
    set: Option<bool>,
) -> Result<(), DdlError> {
    // `pg_database_owner` is never a member of any role.
    if interp.roles.name(member) == "pg_database_owner" {
        return Err(DdlError::Parse(
            "role \"pg_database_owner\" cannot be a member of any role".into(),
        ));
    }
    let grantor = interp.roles.canonical(grantor);
    // Loop check, ignoring superuser status (`is_member_of_role_nosuper`).
    if interp.roles.is_member_nosuper(role, member) {
        return Err(DdlError::Parse(format!(
            "role \"{}\" is a member of role \"{}\"",
            interp.roles.name(role),
            interp.roles.name(member)
        )));
    }
    if let Some(existing) = interp
        .roles
        .memberships
        .iter_mut()
        .find(|m| m.role == role && m.member == member && m.grantor == grantor)
    {
        if let Some(admin) = admin {
            existing.admin = admin;
        }
        if let Some(inherit) = inherit {
            existing.inherit = inherit;
        }
        if let Some(set) = set {
            existing.set = set;
        }
        return Ok(());
    }
    let member_inherit = interp.roles.get(member).is_none_or(|r| r.inherit);
    let oid = interp.alloc_oid()?.get();
    interp.roles.memberships.push(Membership {
        role,
        member,
        grantor,
        admin: admin.unwrap_or(false),
        inherit: inherit.unwrap_or(member_inherit),
        set: set.unwrap_or(true),
        oid,
    });
    Ok(())
}

/// The `GRANT` / `REVOKE` options of a `GrantRoleStmt` (`grant_role_opt`):
/// `(admin, inherit, set)`, each only when specified.
type RoleGrantOptions = (Option<bool>, Option<bool>, Option<bool>);

fn role_grant_options(
    interp: &PgCatalog,
    stmt: &typedpg_pg_query::ast::GrantRoleStmt,
) -> Result<RoleGrantOptions, DdlError> {
    let mut options = (None, None, None);
    for opt in &stmt.opt {
        let Some(ast::Node::DefElem(de)) = opt.as_ref() else {
            continue;
        };
        let value = option_bool(interp, de)?;
        match de.defname.as_str() {
            "admin" => options.0 = Some(value),
            "inherit" => options.1 = Some(value),
            "set" => options.2 = Some(value),
            other => {
                return Err(DdlError::Parse(format!(
                    "unrecognized role option \"{other}\""
                )));
            }
        }
    }
    Ok(options)
}

/// The name of `granted_roles`-style entries: `AccessPriv.priv_name`.
fn granted_role_name(node: &ast::Node) -> Option<String> {
    match node {
        ast::Node::AccessPriv(ap) => Some(ap.priv_name.clone()),
        ast::Node::RoleSpec(rs) => Some(rs.rolename.clone()),
        _ => None,
    }
}

/// `CREATE ROLE` / `CREATE USER` / `CREATE GROUP` (`CreateRole`).
pub fn create_role(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::ast::CreateRoleStmt,
) -> Result<(), DdlError> {
    let name = stmt.role.clone();
    check_new_role_name(&name)?;
    if interp.roles.exists(&name) {
        return Err(DdlError::DuplicateObject(format!(
            "role \"{name}\" already exists"
        )));
    }
    let stmt_type = stmt.stmt_type;
    let id = {
        let id = interp.roles.add(&name, true, false);
        let role = interp.roles.roles.get_mut(&id).expect("just inserted");
        role.login = stmt_type == RoleStmtType::RolestmtUser;
        id
    };
    // The memberships the options create are recorded like a GRANT's: a
    // superuser's grants as the bootstrap superuser (`check_role_grantor`),
    // so the same user's REVOKE removes them.
    let current = current_role(interp);
    let grantor = if interp.roles.is_superuser(current) {
        interp.roles.bootstrap
    } else {
        current
    };
    // `IN ROLE g` (addroleto): the new role joins g. `ROLE g` / `ADMIN g`
    // (rolemembers / adminmembers): g joins the new role.
    let mut add_to = Vec::new();
    let mut members = Vec::new();
    let mut admin_of = Vec::new();
    for opt in &stmt.options {
        let Some(ast::Node::DefElem(de)) = opt.as_ref() else {
            continue;
        };
        match de.defname.as_str() {
            "superuser" => {
                let value = option_bool(interp, de)?;
                if let Some(role) = interp.roles.roles.get_mut(&id) {
                    role.superuser = value;
                }
            }
            "inherit" => {
                let value = option_bool(interp, de)?;
                if let Some(role) = interp.roles.roles.get_mut(&id) {
                    role.inherit = value;
                }
            }
            "bypassrls" => {
                let value = option_bool(interp, de)?;
                if let Some(role) = interp.roles.roles.get_mut(&id) {
                    role.bypassrls = value;
                }
            }
            "canlogin" => {
                let value = option_bool(interp, de)?;
                if let Some(role) = interp.roles.roles.get_mut(&id) {
                    role.login = value;
                }
            }
            // Options the model stores nothing for, and that don't have a
            // boolean value (a password or `VALID UNTIL` timestamp).
            "password" | "validUntil" => {}
            "createrole" => {
                let value = option_bool(interp, de)?;
                if let Some(role) = interp.roles.roles.get_mut(&id) {
                    role.createrole = value;
                }
            }
            "createdb" => {
                let value = option_bool(interp, de)?;
                if let Some(role) = interp.roles.roles.get_mut(&id) {
                    role.createdb = value;
                }
            }
            "isreplication" | "connectionlimit" | "sysid" => {
                let _ = option_bool(interp, de);
            }
            // `ROLE g` / `IN ROLE g` / `ADMIN g`.
            "rolemembers" => members.extend(role_list(de)),
            "addroleto" => add_to.extend(role_list(de)),
            "adminmembers" => admin_of.extend(role_list(de)),
            other => {
                return Err(DdlError::Parse(format!(
                    "unrecognized role option \"{other}\""
                )));
            }
        }
    }
    for spec in add_to {
        let Some(granted_name) = role_spec_name(interp, spec) else {
            return Err(DdlError::TypeNotFound(
                "role \"public\" does not exist".into(),
            ));
        };
        if !session::role_may_exist(&granted_name) {
            return Err(DdlError::TypeNotFound(format!(
                "role \"{granted_name}\" does not exist"
            )));
        }
        let granted = interp.roles.id_or_intern(&granted_name);
        // `IN ROLE g`: the new role is a member of g.
        add_membership(interp, granted, id, grantor, None, None, None)?;
    }
    for spec in admin_of {
        let Some(member_name) = role_spec_name(interp, spec) else {
            return Err(DdlError::TypeNotFound(
                "role \"public\" does not exist".into(),
            ));
        };
        let member = interp.roles.id_or_intern(&member_name);
        add_membership(interp, id, member, grantor, Some(true), None, None)?;
    }
    for spec in members {
        let Some(member_name) = role_spec_name(interp, spec) else {
            return Err(DdlError::TypeNotFound(
                "role \"public\" does not exist".into(),
            ));
        };
        let member = interp.roles.id_or_intern(&member_name);
        add_membership(interp, id, member, grantor, None, None, None)?;
    }
    Ok(())
}

/// `ALTER ROLE` / `ALTER USER` / `ALTER GROUP`: attributes, or `RENAME TO`
/// (`AlterRole`).
pub fn alter_role(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::ast::AlterRoleStmt,
) -> Result<(), DdlError> {
    let Some(spec) = stmt.role.as_ref() else {
        return Ok(());
    };
    let id = resolve_role_spec(interp, spec)?;
    if stmt.action < 0 {
        // RENAME TO: the new name comes as a `newname` option.
        let new_name = stmt.options.iter().find_map(|o| match o.as_ref() {
            Some(ast::Node::DefElem(de)) if de.defname == "newname" => match de.arg.as_ref() {
                Some(ast::Node::String(s)) => Some(s.sval.clone()),
                _ => None,
            },
            _ => None,
        });
        let Some(new_name) = new_name else {
            return Ok(());
        };
        check_new_role_name(&new_name)?;
        if interp.roles.exists(&new_name) {
            return Err(DdlError::DuplicateObject(format!(
                "role \"{new_name}\" already exists"
            )));
        }
        interp.roles.rename(id, new_name);
        return Ok(());
    }
    for opt in &stmt.options {
        let Some(ast::Node::DefElem(de)) = opt.as_ref() else {
            continue;
        };
        match de.defname.as_str() {
            "superuser" => {
                let value = option_bool(interp, de)?;
                if let Some(role) = interp.roles.roles.get_mut(&id) {
                    role.superuser = value;
                }
            }
            "inherit" => {
                let value = option_bool(interp, de)?;
                if let Some(role) = interp.roles.roles.get_mut(&id) {
                    role.inherit = value;
                }
            }
            "bypassrls" => {
                let value = option_bool(interp, de)?;
                if let Some(role) = interp.roles.roles.get_mut(&id) {
                    role.bypassrls = value;
                }
            }
            "canlogin" => {
                let value = option_bool(interp, de)?;
                if let Some(role) = interp.roles.roles.get_mut(&id) {
                    role.login = value;
                }
            }
            "password" | "validUntil" => {}
            "createrole" => {
                let value = option_bool(interp, de)?;
                if let Some(role) = interp.roles.roles.get_mut(&id) {
                    role.createrole = value;
                }
            }
            "createdb" => {
                let value = option_bool(interp, de)?;
                if let Some(role) = interp.roles.roles.get_mut(&id) {
                    role.createdb = value;
                }
            }
            "isreplication" | "connectionlimit" | "sysid" => {
                let _ = option_bool(interp, de);
            }
            other => {
                return Err(DdlError::Parse(format!(
                    "unrecognized role option \"{other}\""
                )));
            }
        }
    }
    Ok(())
}

/// `GRANT role [, ...] TO role [, ...] [WITH ...]` / `REVOKE ...`
/// (`GrantRole` / `RevokeRole`).
pub fn grant_role(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::ast::GrantRoleStmt,
) -> Result<(), DdlError> {
    let current = current_role(interp);
    let (admin, inherit, set) = role_grant_options(interp, stmt)?;
    let explicit_grantor = match stmt.grantor.as_ref() {
        Some(spec) => Some(resolve_role_spec(interp, spec)?),
        None => None,
    };
    let mut granted = Vec::new();
    for node in &stmt.granted_roles {
        let Some(name) = node.as_ref().and_then(granted_role_name) else {
            continue;
        };
        if !session::role_may_exist(&name) {
            return Err(DdlError::TypeNotFound(format!(
                "role \"{name}\" does not exist"
            )));
        }
        granted.push(interp.roles.id_or_intern(&name));
    }
    let mut members = Vec::new();
    for node in &stmt.grantee_roles {
        let Some(ast::Node::RoleSpec(spec)) = node.as_ref() else {
            continue;
        };
        members.push(resolve_role_spec(interp, spec)?);
    }
    let behavior = stmt.behavior;
    let cascade = behavior == DropBehavior::DropCascade;
    for &role in &granted {
        // `check_role_membership_authorization`.
        if stmt.is_grant && interp.roles.name(role) == "pg_database_owner" {
            return Err(DdlError::Parse(
                "role \"pg_database_owner\" cannot have explicit members".into(),
            ));
        }
        let grantor = check_role_grantor(interp, current, role, explicit_grantor, stmt.is_grant)?;
        for &member in &members {
            if stmt.is_grant {
                add_membership(interp, role, member, grantor, admin, inherit, set)?;
                continue;
            }
            // REVOKE: only the grants this grantor recorded. With an option
            // named (`REVOKE ADMIN/INHERIT/SET OPTION FOR`), only that
            // option is cleared; otherwise the membership goes, with the
            // grants that depended on its ADMIN OPTION
            // (`plan_single_revoke`).
            revoke_membership(
                interp,
                role,
                member,
                grantor,
                &(admin, inherit, set),
                cascade,
            )?;
        }
    }
    Ok(())
}

/// The option a REVOKE names (`REVOKE ADMIN/INHERIT/SET OPTION FOR`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RevokeOption {
    Admin,
    Inherit,
    Set,
}

fn named_revoke_option(options: RoleGrantOptions) -> Option<RevokeOption> {
    match options {
        (Some(false), _, _) => Some(RevokeOption::Admin),
        (_, Some(false), _) => Some(RevokeOption::Inherit),
        (_, _, Some(false)) => Some(RevokeOption::Set),
        _ => None,
    }
}

/// One planned action from `plan_recursive_revoke`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RevokeAction {
    Noop,
    RemoveAdmin,
    RemoveInherit,
    RemoveSet,
    Delete,
}

/// `plan_recursive_revoke` (user.c): plan the deletion of one grant and,
/// when it removes the member's ADMIN OPTION and no other grant leaves it
/// that option, of the grants that depended on it.
fn plan_recursive_revoke(
    rows: &[Membership],
    actions: &mut [RevokeAction],
    index: usize,
    revoke_admin_option_only: bool,
    cascade: bool,
) -> Result<(), DdlError> {
    if actions[index] == RevokeAction::Delete {
        return Ok(());
    }
    if actions[index] == RevokeAction::RemoveAdmin && revoke_admin_option_only {
        return Ok(());
    }
    let row = rows[index];
    if !revoke_admin_option_only {
        actions[index] = RevokeAction::Delete;
        if !row.admin {
            return Ok(());
        }
    } else {
        if !row.admin {
            return Ok(());
        }
        actions[index] = RevokeAction::RemoveAdmin;
    }
    // Determine whether the member would still have ADMIN OPTION.
    let still_admin = rows.iter().enumerate().any(|(i, candidate)| {
        candidate.member == row.member && candidate.admin && actions[i] == RevokeAction::Noop
    });
    if still_admin {
        return Ok(());
    }
    for (i, candidate) in rows.iter().enumerate() {
        if candidate.grantor == row.member && actions[i] != RevokeAction::Delete {
            if !cascade {
                return Err(DdlError::DependencyError(
                    "dependent privileges exist (Use CASCADE to revoke them too.)".into(),
                ));
            }
            plan_recursive_revoke(rows, actions, i, false, cascade)?;
        }
    }
    Ok(())
}

/// `plan_single_revoke`: plan one REVOKE. `false` when no grant of the
/// grantor to the member exists (PG warns and changes nothing).
fn plan_single_revoke(
    rows: &[Membership],
    actions: &mut [RevokeAction],
    member: RoleId,
    grantor: RoleId,
    options: RoleGrantOptions,
    cascade: bool,
) -> Result<bool, DdlError> {
    let Some(index) = rows
        .iter()
        .position(|row| row.member == member && row.grantor == grantor)
    else {
        return Ok(false);
    };
    match named_revoke_option(options) {
        // Revoking INHERIT / SET doesn't affect dependent grants.
        Some(RevokeOption::Inherit) => actions[index] = RevokeAction::RemoveInherit,
        Some(RevokeOption::Set) => actions[index] = RevokeAction::RemoveSet,
        option => plan_recursive_revoke(
            rows,
            actions,
            index,
            option == Some(RevokeOption::Admin),
            cascade,
        )?,
    }
    Ok(true)
}

/// `DelRoleMems` for one member of `role`.
fn revoke_membership(
    interp: &mut PgCatalog,
    role: RoleId,
    member: RoleId,
    grantor: RoleId,
    options: &RoleGrantOptions,
    cascade: bool,
) -> Result<(), DdlError> {
    let indices: Vec<usize> = interp
        .roles
        .memberships
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == role)
        .map(|(i, _)| i)
        .collect();
    let rows: Vec<Membership> = indices
        .iter()
        .map(|&i| interp.roles.memberships[i])
        .collect();
    let mut actions = vec![RevokeAction::Noop; rows.len()];
    if !plan_single_revoke(&rows, &mut actions, member, grantor, *options, cascade)? {
        // PG: `role "..." has not been granted membership in role "..." by
        // role "..."` (warning).
        return Ok(());
    }
    // Apply from the end so removing an entry never invalidates a later
    // index.
    for (row, action) in indices.iter().zip(actions.iter()).rev() {
        match action {
            RevokeAction::Noop => {}
            RevokeAction::Delete => {
                interp.roles.memberships.remove(*row);
            }
            RevokeAction::RemoveAdmin => interp.roles.memberships[*row].admin = false,
            RevokeAction::RemoveInherit => interp.roles.memberships[*row].inherit = false,
            RevokeAction::RemoveSet => interp.roles.memberships[*row].set = false,
        }
    }
    Ok(())
}

/// `check_role_grantor` (user.c): the grantor a GRANT / REVOKE of role
/// membership is recorded as made by. Without `GRANTED BY`: a superuser's
/// grants as the bootstrap superuser, another role's as the closest role
/// whose ADMIN OPTION it inherits (itself first). An explicit grantor must be
/// a role whose privileges the current role has, and — for GRANT — one that
/// holds ADMIN OPTION on the granted role.
fn check_role_grantor(
    interp: &PgCatalog,
    current: RoleId,
    role: RoleId,
    explicit: Option<RoleId>,
    is_grant: bool,
) -> Result<RoleId, DdlError> {
    let Some(grantor) = explicit else {
        if interp.roles.is_superuser(current) {
            return Ok(interp.roles.bootstrap);
        }
        return Ok(interp
            .roles
            .select_best_admin(current, role)
            .unwrap_or(current));
    };
    // `CURRENT_USER` is the migrations' user, which is the bootstrap
    // superuser: PG records the bootstrap and exempts it from the ADMIN
    // check (`check_role_grantor`'s `grantorId != BOOTSTRAP_SUPERUSERID`).
    let grantor = interp.roles.canonical(grantor);
    let grantor_name = interp.roles.name(grantor);
    if is_grant {
        if !interp.roles.has_privs_of_role(current, grantor) {
            return Err(DdlError::Parse(format!(
                "permission denied to grant privileges as role \"{grantor_name}\" (Only roles \
                 with privileges of role \"{grantor_name}\" may grant privileges as this role.)"
            )));
        }
        if grantor != interp.roles.bootstrap
            && interp.roles.select_best_admin(grantor, role) != Some(grantor)
        {
            return Err(DdlError::Parse(format!(
                "permission denied to grant privileges as role \"{grantor_name}\" (The grantor \
                 must have the ADMIN option on role \"{}\".)",
                interp.roles.name(role)
            )));
        }
    } else if !interp.roles.has_privs_of_role(current, grantor) {
        return Err(DdlError::Parse(format!(
            "permission denied to revoke privileges granted by role \"{grantor_name}\" (Only \
             roles with privileges of role \"{grantor_name}\" may revoke privileges granted by \
             this role.)"
        )));
    }
    Ok(grantor)
}

/// `DROP ROLE [IF EXISTS] a [, ...]` (`DropRole`): a role that still owns
/// objects or holds grants can't go.
pub fn drop_role(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::ast::DropRoleStmt,
) -> Result<(), DdlError> {
    for node in &stmt.roles {
        let Some(ast::Node::RoleSpec(spec)) = node.as_ref() else {
            continue;
        };
        let Some(name) = role_spec_name(interp, spec) else {
            return Err(DdlError::Parse(
                "cannot use special role specifier in DROP ROLE".into(),
            ));
        };
        let Some(id) = interp.roles.id(&name).map(|id| interp.roles.canonical(id)) else {
            if stmt.missing_ok || session::role_may_exist(&name) {
                // A name the migrations never created is an external role:
                // taken to exist, with nothing the model knows how to drop.
                continue;
            }
            return Err(DdlError::TypeNotFound(format!(
                "role \"{name}\" does not exist"
            )));
        };
        if id == interp.roles.bootstrap && interp.session_identity.current_user().is_none() {
            // The mirror's session user is the bootstrap superuser.
            return Err(DdlError::Parse("current user cannot be dropped".into()));
        }
        if !interp.roles.get(id).is_some_and(|r| r.created) {
            // Predefined roles are pinned; the bootstrap superuser still owns
            // the built-in objects.
            if session::PREDEFINED_ROLES.contains(&name.as_str()) {
                return Err(DdlError::DependencyError(format!(
                    "cannot drop role {name} because it is required by the database system"
                )));
            }
            if stmt.missing_ok {
                continue;
            }
            if id == interp.roles.bootstrap {
                return Err(DdlError::DependencyError(format!(
                    "role \"{name}\" cannot be dropped because some objects depend on it"
                )));
            }
            // An external role: accepted, nothing known to drop.
            continue;
        }
        if let Some(details) = super::acl::role_dependency_details(interp, id) {
            return Err(DdlError::DependencyError(format!(
                "role \"{name}\" cannot be dropped because some objects depend on it ({details})"
            )));
        }
        interp.roles.remove(id);
        interp.acls.forget_role(id);
    }
    Ok(())
}

/// `ALTER ROLE ... RENAME TO` (a `RenameStmt` with `OBJECT_ROLE`).
pub fn rename_role(interp: &mut PgCatalog, old: &str, new_name: &str) -> Result<(), DdlError> {
    check_role_name_chars(new_name)?;
    let old = if old.is_empty() {
        interp
            .session_identity
            .current_user()
            .unwrap_or(PgCatalog::MIGRATIONS_USER)
            .to_owned()
    } else {
        old.to_owned()
    };
    if !session::role_may_exist(&old) && old != PgCatalog::MIGRATIONS_USER {
        return Err(DdlError::TypeNotFound(format!(
            "role \"{old}\" does not exist"
        )));
    }
    check_new_role_name(new_name)?;
    if interp.roles.exists(new_name) && new_name != old {
        return Err(DdlError::DuplicateObject(format!(
            "role \"{new_name}\" already exists"
        )));
    }
    let id = interp.roles.id_or_intern(&old);
    if new_name != old {
        // A stored `regrole` constant names the role.
        super::stored_exprs::follow_rename(
            interp,
            super::stored_exprs::Renamed::Role {
                old: &old,
                new: new_name,
            },
        );
        interp.roles.rename(id, new_name.to_owned());
    }
    Ok(())
}

/// `IsPinnedObject(AuthIdRelationId, roleid)`: the bootstrap superuser and
/// the predefined roles were created by initdb, so `REASSIGN OWNED` and
/// `DROP OWNED` refuse them (`shdepReassignOwned` / `shdepDropOwned`). The
/// migrations' user is the bootstrap superuser, so it is pinned too.
fn check_role_not_pinned(interp: &PgCatalog, role: RoleId, action: &str) -> Result<(), DdlError> {
    let role = interp.roles.canonical(role);
    let name = interp.roles.name(role);
    if role == interp.roles.bootstrap || session::PREDEFINED_ROLES.contains(&name) {
        return Err(DdlError::DependencyError(format!(
            "cannot {action} objects owned by role {name} because they are required by the \
             database system"
        )));
    }
    Ok(())
}

/// `REASSIGN OWNED BY a [, ...] TO b`: the ownership of everything the roles
/// own moves to `b`, with their ACLs' owner/grantor mentions (`aclnewowner`).
pub fn reassign_owned(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::ast::ReassignOwnedStmt,
) -> Result<(), DdlError> {
    let Some(new_spec) = stmt.newrole.as_ref() else {
        return Ok(());
    };
    let new_owner = resolve_role_spec(interp, new_spec)?;
    let new_owner = interp.roles.canonical(new_owner);
    let mut old = Vec::new();
    for node in &stmt.roles {
        let Some(ast::Node::RoleSpec(spec)) = node.as_ref() else {
            continue;
        };
        old.push(resolve_role_spec(interp, spec)?);
    }
    for role in old {
        check_role_not_pinned(interp, role, "reassign ownership of")?;
        let role = interp.roles.canonical(role);
        interp.acls.reassign_owner(role, new_owner);
    }
    Ok(())
}

/// `DROP OWNED BY a [, ...] [CASCADE | RESTRICT]`: drops what the roles own
/// and revokes the grants made to (or by) them.
pub fn drop_owned(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::ast::DropOwnedStmt,
) -> Result<(), DdlError> {
    let mut roles = Vec::new();
    for node in &stmt.roles {
        let Some(ast::Node::RoleSpec(spec)) = node.as_ref() else {
            continue;
        };
        roles.push(resolve_role_spec(interp, spec)?);
    }
    let cascade = stmt.behavior == DropBehavior::DropCascade;
    let mut canonical: Vec<RoleId> = Vec::new();
    for role in roles {
        check_role_not_pinned(interp, role, "drop")?;
        let role = interp.roles.canonical(role);
        if !canonical.contains(&role) {
            canonical.push(role);
        }
    }
    // `shdepDropOwned` collects every role's objects into one set before
    // `performMultipleDeletions`: a RESTRICT check sees dependencies among
    // what the command deletes together, so two roles' objects depending on
    // each other don't block it.
    if !cascade {
        let mut targets = Vec::new();
        for &role in &canonical {
            super::acl::owned_targets(interp, role, &mut targets);
        }
        if let Some((blocker, via)) = super::depend::multiple_deletion_blockers(interp, &targets)
            .into_iter()
            .next()
        {
            return Err(DdlError::DependencyError(format!(
                "cannot drop desired object(s) because other objects depend on them ({} depends \
                 on {})",
                super::depend::describe_dependent(interp, blocker),
                super::depend::describe(interp, via)
            )));
        }
    }
    for role in canonical {
        super::acl::drop_owned_by(interp, role, cascade, true)?;
    }
    Ok(())
}

impl PgCatalog {
    /// Whether `name` is a role the catalog knows: one a migration created,
    /// a predefined one, the bootstrap superuser, or the migrations' own
    /// user. Roles the migrations only mention (a grant to `app_role` that no
    /// `CREATE ROLE` introduces) are external and not *known*: the model
    /// holds their grants, but cannot simulate their attributes or
    /// membership.
    pub fn role_exists(&self, name: &str) -> bool {
        let Some(id) = self.roles.id(name) else {
            return false;
        };
        let Some(role) = self.roles.get(id) else {
            return false;
        };
        role.created
            || id == self.roles.bootstrap
            || id == self.roles.migrations_user
            || session::PREDEFINED_ROLES.contains(&role.name.as_str())
    }

    /// The roles the migrations created, oldest first. The `pg_sanity` oracle
    /// compares their privileges and drops them at teardown.
    #[cfg(feature = "pg_sanity")]
    pub(crate) fn created_roles(&self) -> Vec<String> {
        self.roles.created_names()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `roles_is_member_of` gives the database owner `pg_database_owner`'s
    /// implicit membership; the model's database belongs to the migrations'
    /// user (the bootstrap superuser).
    #[test]
    fn database_owner_is_implicitly_a_member_of_pg_database_owner() {
        let roles = Roles::new();
        let database_owner = roles.id("pg_database_owner").expect("predefined role");
        assert!(
            roles
                .roles_with_privs_of(roles.migrations_user)
                .contains(&database_owner)
        );
        assert!(
            roles
                .roles_with_privs_of(roles.bootstrap)
                .contains(&database_owner)
        );
        // An ordinary role doesn't get it.
        let other = roles.id("pg_read_all_data").expect("predefined role");
        assert!(!roles.roles_with_privs_of(other).contains(&database_owner));
    }

    #[test]
    fn the_migrations_user_is_the_bootstrap_superuser() {
        let roles = Roles::new();
        assert!(roles.same_real_role(roles.migrations_user, roles.bootstrap));
        assert_eq!(roles.canonical(roles.migrations_user), roles.bootstrap);
        assert_eq!(roles.display_name(roles.migrations_user), "postgres");
    }
}
