//! What the catalog doesn't know for certain: the state code the analyzer
//! could not follow may have changed.
//!
//! A `DO` block or a procedure whose control flow depends on run-time values
//! (a loop over a table's rows, an `EXECUTE` of a computed string, a
//! condition the analyzer can't decide) may or may not have done what its
//! statements say. The analyzer keeps what certainly ran and records, for
//! the rest, what it *may* have changed ([`Uncertain`]): the privileges of
//! the roles it may have granted to or revoked from, the memberships it may
//! have granted or revoked, the roles it may have created, altered or
//! dropped, the routines it may have redefined — or, when even the kind of
//! statement is unknown, the whole catalog.
//!
//! The rule is soundness: nothing the analyzer reports may depend on what
//! is uncertain. Privilege checks read the catalog at its *upper bound*
//! ([`Bound::Upper`]: every privilege and membership that may have been
//! granted is held, every one that may have been revoked is kept), so a
//! check that may pass does; the `pg_sanity` oracles read both bounds and
//! accept whatever PostgreSQL holds in between.

use std::collections::BTreeSet;

/// A set of roles, by name: some roles, or every role.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RoleScope {
    pub(crate) all: bool,
    pub(crate) names: BTreeSet<String>,
}

impl RoleScope {
    pub(crate) fn every() -> Self {
        RoleScope {
            all: true,
            names: BTreeSet::new(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        !self.all && self.names.is_empty()
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.all || self.names.contains(name)
    }

    pub(crate) fn add(&mut self, name: &str) {
        if !self.all {
            self.names.insert(name.to_owned());
        }
    }

    pub(crate) fn add_all(&mut self) {
        self.all = true;
        self.names.clear();
    }

    pub(crate) fn merge(&mut self, other: &RoleScope) {
        if other.all {
            self.add_all();
        } else {
            for name in &other.names {
                self.add(name);
            }
        }
    }
}

/// What code the analyzer couldn't follow may have changed. Empty when the
/// catalog is certain.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Uncertain {
    /// Roles that may hold privileges (on any object, with grant option)
    /// the catalog doesn't record: grantees of a `GRANT` that may have run,
    /// new owners of an `ALTER ... OWNER` that may have run.
    pub(crate) gained_privileges: RoleScope,
    /// Roles whose recorded privileges may be gone: grantees of a `REVOKE`
    /// that may have run.
    pub(crate) lost_privileges: RoleScope,
    /// Memberships that may exist although the catalog lacks them: `(role,
    /// member)` scopes, every role of the first a member's of every role of
    /// the second (with every option).
    pub(crate) gained_memberships: Vec<(RoleScope, RoleScope)>,
    /// Recorded memberships that may be gone, as `(role, member)` scopes.
    pub(crate) lost_memberships: Vec<(RoleScope, RoleScope)>,
    /// Roles that may exist, not exist, or have other attributes or
    /// settings than the catalog records.
    pub(crate) roles: RoleScope,
    /// Roles that may own objects other than the ones the catalog says, or
    /// may no longer own them.
    pub(crate) owners: RoleScope,
    /// The default privileges (`pg_default_acl`) may differ.
    pub(crate) default_acls: bool,
    /// Existing routines may have been redefined (`CREATE OR REPLACE` of a
    /// definition the analyzer can't see).
    pub(crate) routines: bool,
    /// Triggers may have been enabled or disabled (`tgenabled`), but for
    /// the foreign keys' own.
    pub(crate) triggers: bool,
    /// Custom settings (`prefix.name`) may hold values the analyzer doesn't
    /// know: a `set_config` or `SET` of one may have run.
    pub(crate) settings: bool,
    /// Anything may differ: a statement of unknown kind may have run.
    pub(crate) catalog: bool,
}

impl Uncertain {
    pub(crate) fn is_empty(&self) -> bool {
        *self == Uncertain::default()
    }

    /// Whether what a privilege check reads may differ from the catalog:
    /// privileges, memberships, role attributes, owners, default ACLs.
    #[cfg_attr(not(feature = "pg_sanity"), allow(dead_code))]
    pub(crate) fn privileges(&self) -> bool {
        !self.gained_privileges.is_empty()
            || !self.lost_privileges.is_empty()
            || !self.gained_memberships.is_empty()
            || !self.lost_memberships.is_empty()
            || !self.roles.is_empty()
            || !self.owners.is_empty()
            || self.default_acls
            || self.catalog
    }

    pub(crate) fn merge(&mut self, other: &Uncertain) {
        self.gained_privileges.merge(&other.gained_privileges);
        self.lost_privileges.merge(&other.lost_privileges);
        for pair in &other.gained_memberships {
            if !self.gained_memberships.contains(pair) {
                self.gained_memberships.push(pair.clone());
            }
        }
        for pair in &other.lost_memberships {
            if !self.lost_memberships.contains(pair) {
                self.lost_memberships.push(pair.clone());
            }
        }
        self.roles.merge(&other.roles);
        self.owners.merge(&other.owners);
        self.default_acls |= other.default_acls;
        self.routines |= other.routines;
        self.triggers |= other.triggers;
        self.settings |= other.settings;
        self.catalog |= other.catalog;
    }

    /// What may have been granted may also have been revoked, and the
    /// other way round: changes that may have been rolled back.
    pub(crate) fn make_symmetric(&mut self) {
        let gained = self.gained_privileges.clone();
        self.gained_privileges.merge(&self.lost_privileges.clone());
        self.lost_privileges.merge(&gained);
        let mut memberships = self.gained_memberships.clone();
        for pair in &self.lost_memberships {
            if !memberships.contains(pair) {
                memberships.push(pair.clone());
            }
        }
        self.gained_memberships = memberships.clone();
        self.lost_memberships = memberships;
    }

    /// Whether statement `stmt` failing may come from what is uncertain:
    /// anything, once the catalog is; a role or privilege statement, once
    /// privileges, memberships, roles or owners are (a `DROP ROLE` refused
    /// for the privileges a role may no longer hold, a `GRANT` of a grant
    /// option it may hold).
    pub(crate) fn may_fail_on(&self, stmt: &typedpg_pg_query::ast::Node) -> bool {
        use typedpg_pg_query::ast::{Node, ObjectType};
        if self.catalog {
            return true;
        }
        let privileges = !self.gained_privileges.is_empty()
            || !self.lost_privileges.is_empty()
            || !self.gained_memberships.is_empty()
            || !self.lost_memberships.is_empty()
            || !self.roles.is_empty()
            || !self.owners.is_empty()
            || self.default_acls;
        privileges
            && match stmt {
                Node::GrantStmt(_)
                | Node::GrantRoleStmt(_)
                | Node::CreateRoleStmt(_)
                | Node::AlterRoleStmt(_)
                | Node::AlterRoleSetStmt(_)
                | Node::DropRoleStmt(_)
                | Node::DropOwnedStmt(_)
                | Node::ReassignOwnedStmt(_)
                | Node::AlterOwnerStmt(_)
                | Node::AlterDefaultPrivilegesStmt(_) => true,
                Node::RenameStmt(r) => r.rename_type == ObjectType::ObjectRole,
                _ => false,
            }
    }

    /// Whether membership `member` → `role` may be gone.
    pub(crate) fn membership_may_be_lost(&self, role: &str, member: &str) -> bool {
        self.lost_memberships
            .iter()
            .any(|(r, m)| r.contains(role) && m.contains(member))
    }

    /// Whether `member` may be a member of `role` without the catalog
    /// recording it.
    pub(crate) fn membership_may_be_gained(&self, role: &str, member: &str) -> bool {
        self.catalog
            || self
                .gained_memberships
                .iter()
                .any(|(r, m)| r.contains(role) && m.contains(member))
    }

    /// Whether `role`'s membership in other roles, or theirs in it, may
    /// differ from the catalog's.
    #[cfg_attr(not(feature = "pg_sanity"), allow(dead_code))]
    pub(crate) fn memberships_may_differ(&self, role: &str, member: &str) -> bool {
        self.catalog
            || self.roles.contains(role)
            || self.roles.contains(member)
            || self.membership_may_be_gained(role, member)
            || self.membership_may_be_lost(role, member)
    }
}

/// Which bound of an uncertain catalog privilege checks read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bound {
    /// Every privilege and membership that may have been granted is held,
    /// none that may have been revoked is lost: what errors are reported
    /// against (a check that may pass, passes).
    Upper,
    /// Only what certainly holds: every grant that may have been revoked
    /// is gone, none that may have been granted is held.
    Lower,
}

thread_local! {
    static BOUND: std::cell::Cell<Bound> = const { std::cell::Cell::new(Bound::Upper) };
}

/// The bound privilege checks read now.
pub(crate) fn bound() -> Bound {
    BOUND.with(std::cell::Cell::get)
}

/// Run `f` with privilege checks reading `bound`.
#[cfg_attr(not(feature = "pg_sanity"), allow(dead_code))]
pub(crate) fn with_bound<R>(bound: Bound, f: impl FnOnce() -> R) -> R {
    struct Restore(Bound);
    impl Drop for Restore {
        fn drop(&mut self) {
            BOUND.with(|b| b.set(self.0));
        }
    }
    let _restore = Restore(BOUND.with(|b| b.replace(bound)));
    f()
}
