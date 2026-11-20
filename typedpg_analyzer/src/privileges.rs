//! Whether a role may run a statement (#269): what PostgreSQL checks when
//! that role prepares and executes it.
//!
//! The analysis of the statement collects its access requirements here, as
//! [`crate::ddl::depend`] collects its references:
//!
//! - the schemas it names explicitly (`LookupExplicitNamespace`: USAGE);
//! - each relation of its range table, with the privileges the executor's
//!   start requires of it and the columns it reads, inserts and updates
//!   (`RTEPermissionInfo`, checked by `ExecCheckPermissions`);
//! - the routines its plan runs (`ExecInitFunc` / `ExecInitAgg` /
//!   `ExecuteCallStmt`: EXECUTE);
//! - the sequences its `nextval` / `currval` / `setval` calls and the
//!   column defaults it fills reach (checked when they run).
//!
//! [`PgCatalog::check_requirements`] then evaluates them for a role. The
//! collector records nothing outside [`collect`], and nothing while an
//! internal analysis runs inside [`crate::ddl::depend::collect`] (a view's
//! stored query, a function body read for nullability, ...): those
//! references aren't the statement's own range table.
//!
//! Not modeled yet: the bodies of trigger functions and of the routines a
//! statement calls, which run with the caller's privileges; row-level
//! security policies; COPY.

use std::cell::{Cell, RefCell};

use crate::AnalyzeError;
use crate::ddl::acl::{AclObject, bits};
use crate::ddl::depend::Reference;
use crate::error::RawError;
use crate::oid::{PgClassOid, PgNamespaceOid, PgProcOid};
use crate::pg_catalog::{PG_CLASS_RELID, PgCatalog, ProKind, RelKind};

/// How a column of a relation is accessed (`selectedCols`, `insertedCols`,
/// `updatedCols`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColumnAccess {
    Select,
    Insert,
    Update,
}

/// The kind of object an EXECUTE failure names (the `ObjectType` PG passes
/// to `aclcheck_error`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RoutineKind {
    Function,
    Aggregate,
    Procedure,
}

impl RoutineKind {
    fn word(self) -> &'static str {
        match self {
            RoutineKind::Function => "function",
            RoutineKind::Aggregate => "aggregate",
            RoutineKind::Procedure => "procedure",
        }
    }
}

/// A column a statement accesses.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Column {
    Named(String),
    /// A whole-row reference: every column (`InvalidAttrNumber`).
    WholeRow,
}

/// One range-table relation's `RTEPermissionInfo`: what it requires, of
/// whom.
#[derive(Debug, Clone)]
struct RelationRequirement {
    relid: PgClassOid,
    /// `checkAsUser`: `None` for the role running the statement, else the
    /// role a view's query reads its relations as (the view's owner).
    check_as: Option<crate::ddl::roles::RoleId>,
    /// `requiredPerms` (`bits::*`).
    perms: u32,
    selected: Vec<Column>,
    inserted: Vec<Column>,
    updated: Vec<Column>,
    /// Locked by FOR UPDATE / SHARE: a view pushes the lock, and its
    /// UPDATE requirement, into its query's relations.
    locked: bool,
    /// The relation is read in FROM (a scan), rather than only being a
    /// DML target.
    scanned: bool,
}

/// What a statement requires, in the order PG checks it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Requirements {
    schemas: Vec<PgNamespaceOid>,
    relations: Vec<RelationRequirement>,
    routines: Vec<(PgProcOid, RoutineKind)>,
    /// A sequence and the privileges of which any one suffices.
    sequences: Vec<(PgClassOid, u32)>,
    /// Column defaults (or generation expressions) the statement fills,
    /// whose routines and sequences then run.
    defaults: Vec<(PgClassOid, i16)>,
}

impl Requirements {
    fn relation(&mut self, relid: PgClassOid) -> &mut RelationRequirement {
        let i = match self
            .relations
            .iter()
            .position(|r| r.relid == relid && r.check_as.is_none())
        {
            Some(i) => i,
            None => {
                self.relations.push(RelationRequirement {
                    relid,
                    check_as: None,
                    perms: 0,
                    selected: Vec::new(),
                    inserted: Vec::new(),
                    updated: Vec::new(),
                    locked: false,
                    scanned: false,
                });
                self.relations.len() - 1
            }
        };
        &mut self.relations[i]
    }
}

thread_local! {
    static COLLECTOR: RefCell<Option<Requirements>> = const { RefCell::new(None) };
    static SUSPENDED: Cell<u32> = const { Cell::new(0) };
    /// The role [`PgCatalog::analyze_as`] checks the statement against.
    static CHECKING: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Run `f` (an analysis) checking its statement against `role`.
pub(crate) fn as_role<R>(role: &str, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<String>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CHECKING.with(|c| *c.borrow_mut() = self.0.take());
        }
    }
    let outer = CHECKING.with(|c| c.replace(Some(role.to_owned())));
    let _restore = Restore(outer);
    f()
}

/// The role the statement being analyzed is checked against, if any.
pub(crate) fn checking_role() -> Option<String> {
    CHECKING.with(|c| c.borrow().clone())
}

/// Run `f` collecting the access requirements of the statement it
/// analyzes.
pub(crate) fn collect<R>(f: impl FnOnce() -> R) -> (R, Requirements) {
    let outer = COLLECTOR.with(|c| c.replace(Some(Requirements::default())));
    let outer_suspended = SUSPENDED.with(|s| s.replace(0));
    let result = f();
    SUSPENDED.with(|s| s.set(outer_suspended));
    let collected = COLLECTOR.with(|c| c.replace(outer)).unwrap_or_default();
    (result, collected)
}

/// Run `f` without collecting: an analysis of something other than the
/// statement's own range table (see the module's documentation).
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

fn with<F: FnOnce(&mut Requirements)>(f: F) {
    if SUSPENDED.with(Cell::get) > 0 {
        return;
    }
    COLLECTOR.with(|c| {
        if let Some(req) = c.borrow_mut().as_mut() {
            f(req);
        }
    });
}

/// Whether a statement's requirements are being collected.
fn active() -> bool {
    SUSPENDED.with(Cell::get) == 0 && COLLECTOR.with(|c| c.borrow().is_some())
}

/// The statement's range table has relation `relid`, which requires
/// `perms` (`addRangeTableEntry` sets SELECT for a relation read in FROM;
/// the target of a DML statement its command's privilege; row locking
/// UPDATE).
pub(crate) fn note_relation(relid: PgClassOid, perms: u32) {
    with(|req| req.relation(relid).perms |= perms);
}

/// The statement reads relation `relid` in FROM (`addRangeTableEntry`): its
/// rows are scanned, which a view expands into its query.
pub(crate) fn note_scan(relid: PgClassOid) {
    with(|req| req.relation(relid).scanned = true);
}

/// The statement locks relation `relid` (FOR UPDATE / SHARE / ...):
/// `ACL_SELECT_FOR_UPDATE`, which is UPDATE.
pub(crate) fn note_lock(relid: PgClassOid) {
    with(|req| {
        let rel = req.relation(relid);
        rel.perms |= bits::UPDATE;
        rel.locked = true;
    });
}

/// The statement reads column `name` of relation `relid`
/// (`markVarForSelectPriv`), which also requires SELECT of the relation.
pub(crate) fn note_column_by_name(relid: PgClassOid, name: &str) {
    note_column(relid, name, ColumnAccess::Select);
}

/// The statement accesses column `name` of relation `relid`: reads it, or
/// writes it from the INSERT target list or an UPDATE SET list.
pub(crate) fn note_column(relid: PgClassOid, name: &str, access: ColumnAccess) {
    with(|req| {
        let rel = req.relation(relid);
        let (cols, perm) = match access {
            ColumnAccess::Select => (&mut rel.selected, bits::SELECT),
            ColumnAccess::Insert => (&mut rel.inserted, bits::INSERT),
            ColumnAccess::Update => (&mut rel.updated, bits::UPDATE),
        };
        let col = Column::Named(name.to_owned());
        if !cols.contains(&col) {
            cols.push(col);
        }
        rel.perms |= perm;
    });
}

/// A whole-row reference to relation `relid` (`t`, `t.*` in an
/// expression): every column is read.
pub(crate) fn note_whole_row(relid: PgClassOid) {
    with(|req| {
        let rel = req.relation(relid);
        if !rel.selected.contains(&Column::WholeRow) {
            rel.selected.push(Column::WholeRow);
        }
        rel.perms |= bits::SELECT;
    });
}

/// The statement calls routine `oid` — as a window function when
/// `windowed` (`ExecInitWindowAgg` names even an aggregate a function).
/// A call of `nextval` / `currval` / `setval` naming its sequence by a
/// constant also requires the sequence's privileges.
pub(crate) fn note_call(
    snapshot: &PgCatalog,
    oid: PgProcOid,
    windowed: bool,
    args: &[Option<typedpg_pg_query::ast::Node>],
) {
    if !active() {
        return;
    }
    let Some(proc) = snapshot.pg_proc.get(&oid) else {
        return;
    };
    let kind = match proc.prokind {
        ProKind::Aggregate if !windowed => RoutineKind::Aggregate,
        ProKind::Procedure => RoutineKind::Procedure,
        _ => RoutineKind::Function,
    };
    note_routine(oid, kind);
    // nextval / currval / setval (sequence.c): checked as they run.
    let catalog = snapshot.pg_catalog_oid() == Some(proc.pronamespace);
    let needed = match proc.proname.as_str() {
        "nextval" if catalog => bits::USAGE | bits::UPDATE,
        "currval" if catalog => bits::SELECT | bits::USAGE,
        "setval" if catalog => bits::UPDATE,
        _ => return,
    };
    if let Some(seq) = args.first().and_then(|a| constant_regclass(snapshot, a)) {
        note_sequence(seq, needed);
    }
}

/// The relation a constant `regclass` argument names (`'s'`,
/// `'s'::regclass`), as `regclassin` resolves it.
fn constant_regclass(
    snapshot: &PgCatalog,
    arg: &Option<typedpg_pg_query::ast::Node>,
) -> Option<PgClassOid> {
    use typedpg_pg_query::ast::{self};
    let text = match arg.as_ref()? {
        ast::Node::AConst(c) => match c.val.as_ref()? {
            ast::AConstValue::String(s) => s.sval.clone(),
            _ => return None,
        },
        ast::Node::TypeCast(tc) => {
            let names: Vec<&str> = tc
                .type_name
                .as_ref()?
                .names
                .iter()
                .filter_map(|n| match n.as_ref() {
                    Some(ast::Node::String(s)) => Some(s.sval.as_str()),
                    _ => None,
                })
                .collect();
            if names.last() != Some(&"regclass") {
                return None;
            }
            match tc.arg.as_ref()? {
                ast::Node::AConst(c) => match c.val.as_ref()? {
                    ast::AConstValue::String(s) => s.sval.clone(),
                    _ => return None,
                },
                _ => return None,
            }
        }
        _ => return None,
    };
    let parts = crate::reg_input::qualified_name_list(&text).ok()?;
    let (schema, name) = match parts.as_slice() {
        [name] => (None, name.as_str()),
        [schema, name] => (Some(schema.as_str()), name.as_str()),
        _ => return None,
    };
    let class = suspended(|| snapshot.resolve_table(schema, name))?;
    (class.relkind == RelKind::Sequence).then_some(class.oid)
}

/// The statement runs routine `oid`.
pub(crate) fn note_routine(oid: PgProcOid, kind: RoutineKind) {
    with(|req| {
        if !req.routines.iter().any(|(o, _)| *o == oid) {
            req.routines.push((oid, kind));
        }
    });
}

/// The statement names schema `ns` explicitly.
pub(crate) fn note_schema(ns: PgNamespaceOid) {
    with(|req| {
        if !req.schemas.contains(&ns) {
            req.schemas.push(ns);
        }
    });
}

/// The statement runs a sequence function on `seq`, which needs one of
/// `any_of`.
pub(crate) fn note_sequence(seq: PgClassOid, any_of: u32) {
    with(|req| req.sequences.push((seq, any_of)));
}

/// The statement fills column `attnum` of `relid` from its default (or
/// computes its generation expression).
pub(crate) fn note_default(relid: PgClassOid, attnum: i16) {
    with(|req| {
        if !req.defaults.contains(&(relid, attnum)) {
            req.defaults.push((relid, attnum));
        }
    });
}

/// Why a role may not run a statement: PG's error, with what is missing and
/// the grant that would allow it.
struct Denial {
    /// PG's message (`permission denied for table t`).
    message: String,
    /// What is missing, for whom.
    note: String,
    /// The GRANT that would allow it.
    grant: String,
}

impl Denial {
    fn into_error(self) -> AnalyzeError {
        let mut raw = RawError::new(
            AnalyzeError::InsufficientPrivilege(self.message),
            None,
            Some(format!("grant it in a migration: {}", self.grant)),
        );
        raw.notes.push(self.note);
        raw.finalize_implicit()
    }
}

fn quote_role(name: &str) -> String {
    typedpg_core::quote_identifier(name)
}

impl PgCatalog {
    /// The search path of a new session of `role` (recomputeNamespacePath):
    /// its setting's schemas that exist and that the role has USAGE on,
    /// `$user` being the schema named after it.
    pub(crate) fn role_search_path(&self, role: &str) -> Vec<PgNamespaceOid> {
        let id = self.roles.id(role);
        let setting = self.search_path_guc.new_session_of(
            id.and_then(|id| self.roles.search_paths.get(&id))
                .map(Vec::as_slice),
            self.roles.all_search_path.as_deref(),
        );
        let mut out = Vec::new();
        for name in setting {
            let name = match name.as_str() {
                // The application's sessions have no temporary schema.
                "pg_temp" => continue,
                "$user" => role,
                other => other,
            };
            if let Some(oid) = self.namespace_by_name.get(name).copied()
                && Some(oid) != self.temp_namespace
                && !out.contains(&oid)
                && self.has_schema_privilege(role, oid, crate::Privilege::Usage)
            {
                out.push(oid);
            }
        }
        out
    }

    /// Whether `role` may run a statement requiring `req`: PG's checks in
    /// the order it makes them — explicitly named schemas (parse
    /// analysis), the range table (`ExecCheckPermissions`), the routines
    /// the plan runs (executor start), then the sequences the statement's
    /// sequence functions and column defaults reach (as they run).
    pub(crate) fn check_requirements(
        &self,
        role: &str,
        req: &Requirements,
    ) -> Result<(), AnalyzeError> {
        for &ns in &req.schemas {
            if !self.has_schema_privilege(role, ns, crate::Privilege::Usage) {
                let name = self.namespace_name(ns).unwrap_or_default().to_owned();
                return Err(Denial {
                    message: format!("permission denied for schema {name}"),
                    note: format!(
                        "role {} has no USAGE on schema {}",
                        quote_role(role),
                        quote_role(&name)
                    ),
                    grant: format!(
                        "GRANT USAGE ON SCHEMA {} TO {};",
                        quote_role(&name),
                        quote_role(role)
                    ),
                }
                .into_error());
            }
        }

        let mut routines = req.routines.clone();
        let relations = self.expand_views(req, &mut routines);
        for rel in &relations {
            self.check_relation(role, rel)?;
        }

        // Column defaults: their routines run with the statement's.
        let mut sequences = req.sequences.clone();
        for &(relid, attnum) in &req.defaults {
            for d in self.pg_depend.iter().filter(|d| {
                d.classid == PG_CLASS_RELID
                    && d.objid.get() == relid.get()
                    && d.objsubid == attnum
                    && d.deptype == crate::pg_catalog::DepType::Normal
            }) {
                if d.refclassid == crate::pg_catalog::PG_PROC_RELID
                    && let Some(oid) = PgProcOid::new(d.refobjid.get())
                    && !routines.iter().any(|(o, _)| *o == oid)
                {
                    routines.push((oid, RoutineKind::Function));
                }
                if d.refclassid == PG_CLASS_RELID
                    && d.refobjsubid == 0
                    && let Some(seq) = PgClassOid::new(d.refobjid.get())
                    && self
                        .pg_class
                        .get(&seq)
                        .is_some_and(|c| c.relkind == RelKind::Sequence)
                {
                    // A default naming a sequence calls nextval on it
                    // (serial's `nextval('t_id_seq'::regclass)`).
                    sequences.push((seq, bits::USAGE | bits::UPDATE));
                }
            }
        }

        for &(oid, kind) in &routines {
            if !self.has_function_privilege(role, oid) {
                let Some(proc) = self.pg_proc.get(&oid) else {
                    continue;
                };
                let schema = self.namespace_name(proc.pronamespace).unwrap_or_default();
                let args = proc
                    .proargtypes
                    .iter()
                    .map(|&t| crate::ddl::util::format_type_for_message(self, t))
                    .collect::<Vec<_>>()
                    .join(", ");
                let signature = format!(
                    "{}({args})",
                    crate::QualifiedName::new(schema, proc.proname.clone())
                );
                let grant_kind = match kind {
                    RoutineKind::Procedure => "PROCEDURE",
                    _ => "FUNCTION",
                };
                return Err(Denial {
                    message: format!("permission denied for {} {}", kind.word(), proc.proname),
                    note: format!("role {} has no EXECUTE on {signature}", quote_role(role)),
                    grant: format!(
                        "GRANT EXECUTE ON {grant_kind} {signature} TO {};",
                        quote_role(role)
                    ),
                }
                .into_error());
            }
        }

        for &(seq, any_of) in &sequences {
            let allowed = [
                crate::Privilege::Usage,
                crate::Privilege::Select,
                crate::Privilege::Update,
            ]
            .into_iter()
            .filter(|p| p.bit() & any_of != 0)
            .any(|p| self.has_sequence_privilege(role, seq, p));
            if !allowed {
                let Some(class) = self.pg_class.get(&seq) else {
                    continue;
                };
                let schema = self.namespace_name(class.relnamespace).unwrap_or_default();
                let qualified = crate::QualifiedName::new(schema, class.relname.clone());
                let (needed, grant) = if any_of & bits::USAGE != 0 {
                    ("USAGE", "USAGE")
                } else if any_of & bits::UPDATE != 0 {
                    ("UPDATE", "UPDATE")
                } else {
                    ("SELECT", "SELECT")
                };
                return Err(Denial {
                    message: format!("permission denied for sequence {}", class.relname),
                    note: format!(
                        "role {} has no {needed} on sequence {qualified}",
                        quote_role(role)
                    ),
                    grant: format!(
                        "GRANT {grant} ON SEQUENCE {qualified} TO {};",
                        quote_role(role)
                    ),
                }
                .into_error());
            }
        }
        Ok(())
    }

    /// The range table with the views the statement reads expanded, as the
    /// rewriter does (`ApplyRetrieveRule`): a view's own query reads its
    /// relations as the view's owner (`checkAsUser`), or as the statement's
    /// role for a `security_invoker` view, and its routines run as the
    /// statement's role. A view written through (auto-updatable) writes its
    /// base relation as its owner (`rewriteTargetView`).
    fn expand_views(
        &self,
        req: &Requirements,
        routines: &mut Vec<(PgProcOid, RoutineKind)>,
    ) -> Vec<RelationRequirement> {
        let mut out: Vec<RelationRequirement> = Vec::new();
        let mut todo: std::collections::VecDeque<RelationRequirement> =
            req.relations.iter().cloned().collect();
        let mut expanded: Vec<(PgClassOid, Option<crate::ddl::roles::RoleId>)> = Vec::new();
        while let Some(rel) = todo.pop_front() {
            let is_view = self
                .pg_class
                .get(&rel.relid)
                .is_some_and(|c| c.relkind == RelKind::View);
            // The view's query runs when the statement reads the view (in
            // FROM, or locking it), or writes it through an INSTEAD OF
            // trigger for UPDATE / DELETE, which is handed the rows the
            // query finds (ApplyRetrieveRule). A write the rewriter turns
            // into one of the base relation (rewriteTargetView) and an
            // INSERT a trigger takes don't run it.
            let instead_for = |event: i32| {
                self.triggers
                    .get(&rel.relid)
                    .is_some_and(|ts| ts.iter().any(|t| t.is_instead_row_for(event)))
            };
            let runs_query = rel.scanned
                || rel.locked
                || (rel.perms & bits::UPDATE != 0
                    && instead_for(crate::ddl::triggers::TRIGGER_TYPE_UPDATE))
                || (rel.perms & bits::DELETE != 0
                    && instead_for(crate::ddl::triggers::TRIGGER_TYPE_DELETE));
            if is_view && !expanded.contains(&(rel.relid, rel.check_as)) {
                expanded.push((rel.relid, rel.check_as));
                let inner_as = if self.security_invoker_views.contains(&rel.relid) {
                    rel.check_as
                } else {
                    Some(self.owner_of(AclObject::Relation(rel.relid)))
                };
                // The view's query: its relations read, its routines run.
                if runs_query && let Some(refs) = self.view_references.get(&rel.relid) {
                    let mut inner: Vec<RelationRequirement> = Vec::new();
                    let entry = |inner: &mut Vec<RelationRequirement>, relid: PgClassOid| {
                        if let Some(i) = inner.iter().position(|r| r.relid == relid) {
                            i
                        } else {
                            inner.push(RelationRequirement {
                                relid,
                                check_as: inner_as,
                                perms: bits::SELECT | if rel.locked { bits::UPDATE } else { 0 },
                                selected: Vec::new(),
                                inserted: Vec::new(),
                                updated: Vec::new(),
                                locked: rel.locked,
                                scanned: true,
                            });
                            inner.len() - 1
                        }
                    };
                    for r in refs {
                        match r {
                            Reference::Object(a)
                                if a.classid == PG_CLASS_RELID && a.objsubid == 0 =>
                            {
                                if let Some(relid) = PgClassOid::new(a.objid.get())
                                    && self.pg_class.get(&relid).is_some_and(|c| {
                                        !matches!(
                                            c.relkind,
                                            RelKind::Index
                                                | RelKind::PartitionedIndex
                                                | RelKind::CompositeType
                                        )
                                    })
                                {
                                    entry(&mut inner, relid);
                                }
                            }
                            Reference::Object(a)
                                if a.classid == crate::pg_catalog::PG_PROC_RELID =>
                            {
                                if let Some(oid) = PgProcOid::new(a.objid.get())
                                    && !routines.iter().any(|(o, _)| *o == oid)
                                {
                                    let kind = match self.pg_proc.get(&oid).map(|p| p.prokind) {
                                        Some(ProKind::Aggregate) => RoutineKind::Aggregate,
                                        _ => RoutineKind::Function,
                                    };
                                    routines.push((oid, kind));
                                }
                            }
                            Reference::Object(a)
                                if a.classid == crate::pg_catalog::PG_OPERATOR_RELID =>
                            {
                                if let Some(op) = crate::oid::PgOperatorOid::new(a.objid.get())
                                    && let Some(code) =
                                        self.pg_operator.get(&op).and_then(|o| o.oprcode)
                                    && !routines.iter().any(|(o, _)| *o == code)
                                {
                                    routines.push((code, RoutineKind::Function));
                                }
                            }
                            Reference::Column(relid, name) => {
                                let i = entry(&mut inner, *relid);
                                let col = Column::Named(name.clone());
                                if !inner[i].selected.contains(&col) {
                                    inner[i].selected.push(col);
                                }
                            }
                            _ => {}
                        }
                    }
                    todo.extend(inner);
                }
                // An auto-updatable view written through: its base relation
                // takes the write, with the columns mapped. A view with an
                // INSTEAD OF row trigger for the command keeps the write:
                // the trigger handles it and the base relation is not
                // written at all (rewriteHandler's `instead`).
                let mut writes = rel.perms & (bits::INSERT | bits::UPDATE | bits::DELETE);
                let instead = |event: i32| {
                    self.triggers
                        .get(&rel.relid)
                        .is_some_and(|ts| ts.iter().any(|t| t.is_instead_row_for(event)))
                };
                if instead(crate::ddl::triggers::TRIGGER_TYPE_INSERT) {
                    writes &= !bits::INSERT;
                }
                if instead(crate::ddl::triggers::TRIGGER_TYPE_UPDATE) {
                    writes &= !bits::UPDATE;
                }
                if instead(crate::ddl::triggers::TRIGGER_TYPE_DELETE) {
                    writes &= !bits::DELETE;
                }
                let writes = if rel.locked && rel.updated.is_empty() {
                    writes & !bits::UPDATE
                } else {
                    writes
                };
                if writes != 0
                    && let Some(upd) = self.view_updatability.get(&rel.relid)
                    && upd.not_updatable.is_none()
                    && let Some(base) = upd.base
                {
                    let view_attrs = self.attributes_of(rel.relid);
                    let base_attrs = self.attributes_of(base);
                    // A view column's base column, by name.
                    let map = |cols: &[Column]| -> Vec<Column> {
                        cols.iter()
                            .filter_map(|c| match c {
                                Column::WholeRow => Some(Column::WholeRow),
                                Column::Named(name) => {
                                    let a = view_attrs.iter().find(|a| &a.attname == name)?;
                                    let base_num = usize::try_from(a.attnum - 1)
                                        .ok()
                                        .and_then(|i| upd.columns.get(i))
                                        .and_then(|r| r.as_ref().ok().copied())?;
                                    base_attrs
                                        .iter()
                                        .find(|b| b.attnum == base_num)
                                        .map(|b| Column::Named(b.attname.clone()))
                                }
                            })
                            .collect()
                    };
                    todo.push_back(RelationRequirement {
                        relid: base,
                        check_as: inner_as,
                        perms: writes
                            | if rel.selected.is_empty() {
                                0
                            } else {
                                bits::SELECT
                            },
                        selected: map(&rel.selected),
                        inserted: map(&rel.inserted),
                        updated: map(&rel.updated),
                        locked: false,
                        scanned: false,
                    });
                }
            }
            match out
                .iter_mut()
                .find(|r| r.relid == rel.relid && r.check_as == rel.check_as)
            {
                Some(existing) => {
                    existing.perms |= rel.perms;
                    for (from, to) in [
                        (&rel.selected, &mut existing.selected),
                        (&rel.inserted, &mut existing.inserted),
                        (&rel.updated, &mut existing.updated),
                    ] {
                        for c in from {
                            if !to.contains(c) {
                                to.push(c.clone());
                            }
                        }
                    }
                }
                None => out.push(rel),
            }
        }
        out
    }

    /// `ExecCheckOneRelPerms`: the relation-level privileges first; what
    /// they leave of SELECT / INSERT / UPDATE may come from the columns the
    /// statement accesses (all of them; any column when it names none).
    fn check_relation(&self, role: &str, rel: &RelationRequirement) -> Result<(), AnalyzeError> {
        let owner_name;
        let who: &str = match rel.check_as {
            None => role,
            Some(id) => {
                owner_name = self.roles.name(id).to_owned();
                &owner_name
            }
        };
        let Some(class) = self.pg_class.get(&rel.relid) else {
            return Ok(());
        };
        let privileges = [
            crate::Privilege::Select,
            crate::Privilege::Insert,
            crate::Privilege::Update,
            crate::Privilege::Delete,
        ];
        let mut missing_table = Vec::new();
        for p in privileges {
            if rel.perms & p.bit() != 0 && !self.has_table_privilege(who, rel.relid, p) {
                missing_table.push(p);
            }
        }
        if missing_table.is_empty() {
            return Ok(());
        }
        // What the columns can't make up for.
        let mut denied: Vec<(crate::Privilege, Vec<i16>)> = Vec::new();
        for p in missing_table {
            let cols = match p {
                crate::Privilege::Select => &rel.selected,
                crate::Privilege::Insert => &rel.inserted,
                crate::Privilege::Update => &rel.updated,
                _ => {
                    denied.push((p, Vec::new()));
                    continue;
                }
            };
            if cols.is_empty() {
                if !self.has_any_column_privilege(who, rel.relid, p) {
                    denied.push((p, Vec::new()));
                }
                continue;
            }
            let mut lacking = Vec::new();
            for c in cols {
                let attnums: Vec<i16> = match c {
                    // A whole-row reference: every column (ACLMASK_ALL).
                    Column::WholeRow => self
                        .attributes_of(rel.relid)
                        .iter()
                        .filter(|a| a.attnum > 0)
                        .map(|a| a.attnum)
                        .collect(),
                    Column::Named(name) => self
                        .attribute_by_name(rel.relid, name)
                        .map(|a| a.attnum)
                        .or_else(|| {
                            crate::pg_catalog::SYSTEM_COLUMNS
                                .iter()
                                .find(|(n, ..)| n == name)
                                .map(|&(_, _, attnum)| attnum)
                        })
                        .into_iter()
                        .collect(),
                };
                for attnum in attnums {
                    if !self.has_column_privilege(who, rel.relid, attnum, p)
                        && !lacking.contains(&attnum)
                    {
                        lacking.push(attnum);
                    }
                }
            }
            if !lacking.is_empty() {
                denied.push((p, lacking));
            }
        }
        let Some((privilege, columns)) = denied.into_iter().next() else {
            return Ok(());
        };
        // get_relkind_objtype.
        let word = match class.relkind {
            RelKind::View => "view",
            RelKind::MaterializedView => "materialized view",
            RelKind::Sequence => "sequence",
            RelKind::ForeignTable => "foreign table",
            _ => "table",
        };
        let schema = self.namespace_name(class.relnamespace).unwrap_or_default();
        let qualified = crate::QualifiedName::new(schema, class.relname.clone());
        let name = privilege.name();
        let column_names: Vec<String> = columns
            .iter()
            .filter_map(|&n| {
                self.attributes_of(rel.relid)
                    .iter()
                    .find(|a| a.attnum == n)
                    .map(|a| a.attname.clone())
                    .or_else(|| {
                        crate::pg_catalog::SYSTEM_COLUMNS
                            .iter()
                            .find(|(_, _, attnum)| *attnum == n)
                            .map(|(n, ..)| (*n).to_owned())
                    })
            })
            .collect();
        let whom = match rel.check_as {
            None => format!("role {}", quote_role(role)),
            Some(_) => format!(
                "role {} (which the view's query reads as, being the view's owner)",
                quote_role(who)
            ),
        };
        let (note, grant) = if column_names.is_empty() {
            (
                format!("{whom} has no {name} on {qualified}"),
                format!("GRANT {name} ON {qualified} TO {};", quote_role(who)),
            )
        } else {
            let list = column_names
                .iter()
                .map(|c| typedpg_core::quote_identifier(c))
                .collect::<Vec<_>>()
                .join(", ");
            (
                format!("{whom} has no {name} on column(s) {list} of {qualified}"),
                format!(
                    "GRANT {name} ({list}) ON {qualified} TO {};",
                    quote_role(who)
                ),
            )
        };
        Err(Denial {
            message: format!("permission denied for {word} {}", class.relname),
            note,
            grant,
        }
        .into_error())
    }
}
