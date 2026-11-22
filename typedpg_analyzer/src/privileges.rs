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
//!   column defaults it fills reach (checked when they run);
//! - the bodies of the routines it calls or depends on, and of the trigger
//!   functions the writes it makes fire: a body runs with the privileges of
//!   the role the statement runs as, or of the routine's owner under
//!   `SECURITY DEFINER` (`GetUserId`), so what a trigger or a called
//!   function reads and writes is checked too, recursively.
//!
//! # What a statement may run
//!
//! A statement is checked against everything it *may* run, for some data:
//! every trigger that may fire and every body statement that may be
//! reached. Code that provably never runs for this statement is pruned:
//!
//! - `IF` / `ELSIF` / `CASE` / `WHILE` conditions the planner certainly
//!   folds to a constant (`eval_const_expressions`: immutable operators and
//!   functions over constants, a strict function over a NULL) — a cast to a
//!   domain (`CoerceToDomain`) or through a non-immutable cast function is
//!   never folded. The same certainty drops the unreached results of
//!   `CASE`, the `COALESCE` arguments after a non-NULL constant, the
//!   arguments of an `AND` / `OR` after one that decides it, and the
//!   functions of stored expressions (`CHECK`, column defaults and
//!   generated columns, index expressions and predicates, `WHEN`, a
//!   domain's `CHECK` and `DEFAULT`);
//! - conditions on `TG_OP`, `TG_WHEN`, `TG_LEVEL`, `TG_TABLE_NAME`,
//!   `TG_TABLE_SCHEMA`, `TG_RELNAME`, `TG_NAME` compared to string literals
//!   (`=`, `<>`, `!=`, `IN (...)`, `NOT`, `AND`, `OR`, parentheses),
//!   evaluated for the known firing (event, timing, level, relation). A
//!   `TG_*` variable the firing doesn't determine (`TG_NARGS`, `TG_ARGV`,
//!   `TG_RELID`, ...) is UNKNOWN, never NULL, and so is a user variable
//!   named `tg_*` — and any `TG_*` variable the body assigns
//!   (`TG_OP := …`). A trigger function shared by several triggers is walked
//!   once per firing;
//! - statements after an unconditional `RETURN` / `RAISE EXCEPTION` in the
//!   same block, after a nested block (without `EXCEPTION`) whose list ends
//!   so, and after an `IF` whose every branch, `ELSE` included, ends;
//! - statements inside a `BEGIN ... EXCEPTION` block one of whose handlers
//!   catches `insufficient_privilege`, SQLSTATE `'42501'` or `OTHERS` (PG
//!   catches the denial) and doesn't re-raise (`RAISE`, `RAISE EXCEPTION`).
//!   A constraint trigger the block's statement fires runs at commit,
//!   outside the block — but only for the command event that fires it, and
//!   only `DEFERRABLE INITIALLY DEFERRED` (an `INITIALLY IMMEDIATE` one
//!   runs at the end of its statement unless `SET CONSTRAINTS` defers it):
//!   its denial isn't caught. A declared variable's initializer runs before
//!   the handler's protected subtransaction, so its denial isn't caught
//!   either. The handler's own statements are still checked;
//! - `DECLARE` defaults of a block that is never entered (a pruned branch)
//!   don't run; a cursor's query runs only when the cursor is `OPEN`ed or
//!   looped over, not when its block is entered (`OPEN` of an unbound
//!   `refcursor` checks the query it opens, or the expression producing its
//!   dynamic one);
//! - a strict routine called with a certainly NULL constant is NULL before
//!   the executor initializes it: neither the routine's EXECUTE nor any of
//!   its arguments (a default's included) is checked — but the planner
//!   simplifies the arguments *before* applying that shortcut, so an
//!   `IMMUTABLE` call over constants among them runs while planning, and so
//!   does what the constant folding inside a SQL body it inlines runs (the
//!   inlined expression is then dropped: its relations are never planned).
//!   The same applies to an operator's function: an operator call is a call
//!   of its `opfuncid` (`ExecInitFunc`, `evaluate_function`). A
//!   set-returning or `record` result is never folded: the executor
//!   evaluates every argument of such a call before its strictness skips it;
//! - the output columns of a view, FROM subquery or inlined CTE nothing
//!   reads: `pull_up_simple_subquery` never instantiates the expression of
//!   a dropped output, and `remove_unused_subquery_outputs` blanks it in a
//!   separately planned one — so its routines, sequences, sublinks and the
//!   relations those read aren't checked. A volatile expression, a
//!   set-returning one, one a GROUP BY / ORDER BY / DISTINCT ON / window
//!   key names (PG marks it with a `ressortgroupref`), and every output of
//!   a plain `DISTINCT` or of a set operation, is kept. A whole-row or `*`
//!   read reads all of them. A simple `UNION ALL` subquery (no ORDER BY /
//!   LIMIT / locking / WITH, every node a `UNION ALL`, no arm coercion) is
//!   pulled up into an append relation (`pull_up_simple_union_all`), so its
//!   leaves' unused outputs are dropped too. What the parser recorded on a
//!   base relation's `RTEPermissionInfo` (`selectedCols`, checked by
//!   `ExecCheckPermissions`) is *not* dropped with the output — only the
//!   subqueries inside its expression are;
//! - a trigger whose `WHEN` condition certainly folds to `false` / NULL
//!   never fires. Otherwise the condition's functions and operators are
//!   checked as the role running the statement (they run per row, as the
//!   caller), the ones in code the folding drops excepted, and the body is
//!   checked;
//! - when a write goes through an auto-updatable view, only the view's
//!   `WHERE` runs (and, for an INSERT, only under a `CHECK OPTION`,
//!   `CASCADED` propagating to the views below): its select list isn't
//!   evaluated. A view over `ONLY` a relation writes no inheritance child,
//!   and an `INSTEAD OF` trigger for UPDATE / DELETE still needs the
//!   view's query to find the rows it is handed;
//! - a domain's CHECK runs where a `CoerceToDomain` does: assigning a value
//!   already of the column's domain type doesn't coerce it (a cast to the
//!   domain, a value of another type, or a parameter received through the
//!   domain's input function does). A domain's CHECK and DEFAULT
//!   expressions are folded like a stored table expression's. A
//!   partitioned table stores no rows: the generation expression
//!   evaluated is the physical partition's own.
//!
//! Data-dependent branches are still checked: they may run. An UPDATE may
//! be non-HOT for the data (the row needs a page of its own), so every
//! index's expressions and predicate may be evaluated and are checked.
//!
//! A function the planner certainly inlines (a single-expression SQL body,
//! `inline_function`) never evaluates the arguments its body doesn't use:
//! their functions and defaults are not checked.
//!
//! A stored expression (`CHECK`, default, generation expression, index
//! expression) resolves to the overloads that certainly run from the
//! expression itself: its `pg_depend` rows keep the functions of dead code
//! too, and a live call of a name must not resurrect another overload of it.
//! The names were resolved at creation, under the creation `search_path`, so
//! they are never resolved again as the running role: a `pg_depend` routine
//! is pruned only when the same expression's folding walk finds no live call
//! of its name and argument count. A simple `CASE`'s `WHEN` values are
//! compared with its subject under the equality's own types, not tested as
//! conditions (`CASE 1 WHEN '1'` compares integers).
//!
//! An `INSERT` whose partition key values are certainly known is routed
//! (`ExecFindPartition`): the planner prunes the result relations to the
//! partitions the rows reach, and only their generation expressions,
//! constraints and row triggers are initialized.
//!
//! `EXPLAIN` without `ANALYZE` starts the executor and never runs it: the
//! plan's relations are checked (`ExecCheckPermissions`), and the functions
//! of its expressions are resolved — but no expression is evaluated, so a
//! sequence function, a trigger and a routine body are not reached. What
//! the planner itself reaches is: a routine whose body it inlines
//! (`inline_function`, `inline_set_returning_function`) contributes the
//! plan's relations and its routines' EXECUTE, and a call with constant
//! arguments is run while the planner folds it (`evaluate_function`) — an
//! `IMMUTABLE` one anywhere, a `STABLE` one while estimating:
//! the pseudoconstant side of a `WHERE` / `JOIN ... ON` comparison against a
//! relation column, a `LIMIT` / `OFFSET` bound, and a `TABLESAMPLE` method's
//! first parameter (`set_tablesample_rel_size`) — see
//! `estimate_expression_value`. `EXPLAIN ANALYZE` runs the statement and is
//! checked as it.
//!
//! A column default is part of the plan the rewriter builds
//! (`rewriteTargetListIU`): its expression is planned and folded with the
//! statement, an `EXPLAIN` without `ANALYZE` included. So is a domain
//! coercion the rewriter puts in the target list — the values an `INSERT`
//! stores, an `UPDATE ... SET` assigns and a column's or a domain's default
//! produces: its `CoerceToDomain` is initialized with the plan and runs the
//! domain's checks. A `CHECK`, an index expression or partial-index
//! predicate, a stored generation expression, a partition key and a
//! trigger's `WHEN` condition are evaluated per row instead
//! (`ExecConstraints`, index insertion, `ExecComputeStoredGenerated`,
//! `ExecFindPartition`, trigger execution): `EXPLAIN` without `ANALYZE`
//! initializes none of them, so neither their routines' EXECUTE nor their
//! bodies is checked. A constant-only expression among them isn't folded
//! while planning either: it is not part of the plan.
//!
//! # Row-level security
//!
//! When row security applies to a relation (`check_enable_rls`: the table
//! enabled it, and the role the relation is checked for — `checkAsUser`: a
//! view's owner for the relations of its query — is neither its owner (or a
//! role holding the owner's privileges, unless `FORCE ROW LEVEL SECURITY`),
//! nor BYPASSRLS, nor a superuser), the applicable policies' `USING` / `WITH
//! CHECK` expressions become part of the query
//! (`get_row_security_policies`): the statement's result relation applies
//! its command's policies and, when the statement reads it, the SELECT ones;
//! every other relation applies its SELECT ones; a locking read also the
//! UPDATE ones, an `INSERT ... ON CONFLICT DO UPDATE` the UPDATE ones, and a
//! `MERGE` target the INSERT, UPDATE and DELETE ones. The permissive clauses
//! are OR-ed and the restrictive ones AND-ed; a command with no permissive
//! clause gets the default-deny `false` and its restrictive clauses are
//! never built. The policy expressions' functions are initialized with the
//! plan as the invoking role (`ExecInitFunc`), so their EXECUTE and bodies
//! are checked by an `EXPLAIN` without `ANALYZE` too; the relations a policy
//! subquery reads are in the range table and are checked as `checkAsUser`
//! (`setRuleCheckAsUser`), while the policy's own row columns are not the
//! statement's. A stored policy resolves no name at run time: the schemas
//! its expressions name need no USAGE, and its functions are the OIDs its
//! `pg_depend` rows name. The planner's folding prunes what a policy's dead
//! code runs, like any stored expression's.
//!
//! `row_security` off (which makes PG raise an error rather than filter) is
//! part of the role's session settings (`ALTER ROLE` / `ALTER DATABASE ...
//! SET`): everywhere row security would apply, `check_enable_rls` rejects
//! the statement instead. A policy whose expression holds a subquery and
//! reaches its own relation again — directly or through other relations'
//! policies — is rejected with `infinite recursion detected in policy`
//! (`fireRIRrules`); the rewriter doesn't expand routine bodies, so a
//! function call in a policy is not a loop. A stored policy is bound to the
//! objects it was written with, as PG's OIDs are: its tree follows column,
//! relation, routine and schema renames ([`crate::ddl::policies`]), and when
//! it can't be analyzed its `pg_depend` rows still name the routines and
//! relations to check.
//!
//! # Search paths
//!
//! Body statements are analyzed with the privilege collector active, under
//! the effective role and the search path the body sees: its `SET
//! search_path`, or — without one — the search path the enclosing statement
//! or body runs under. A `SECURITY DEFINER` body does not read the owner's
//! `ALTER ROLE ... SET search_path`: the setting in effect at the call site
//! is re-resolved for the owner (`$user` and the USAGE filtering).
//!
//! A SQL-standard body (`BEGIN ATOMIC ... END` / `RETURN ...`) is resolved
//! at CREATE FUNCTION: PG stores it analyzed, so its relations, routines
//! and sequences are the ones the creation search path found, by OID, and
//! running it checks no schema USAGE.
//!
//! A PL/pgSQL body is compiled at its first call: the declared types,
//! `%TYPE` and `%ROWTYPE` it names in `DECLARE` are resolved then, so the
//! schema they name explicitly needs USAGE as the role running the
//! function — whatever the statement that first calls it would do. The
//! types of the function's own signature were resolved at CREATE FUNCTION
//! and need none (the compiler reports which lookups its body's declarations
//! made; see `record_compiled_schemas`). A body's statements resolve their
//! own names when they run, so their schemas are checked where they are
//! walked.
//!
//! Each body is checked once per (routine, effective role, effective search
//! path, trigger firing), so cycles end. A body statement that doesn't
//! analyze — dynamic SQL (`RETURN QUERY EXECUTE`, a construct the analyzer
//! doesn't model, a name it cannot resolve) — is skipped, never reported:
//! only its privileges would be missing. The expression producing a dynamic
//! `EXECUTE`'s text still runs and is checked, as is a dynamic `FOR` loop's
//! static body.
//!
//! [`PgCatalog::check_requirements`] then evaluates them for a role. The
//! collector records nothing outside [`collect`], and nothing while an
//! internal analysis runs inside [`crate::ddl::depend::collect`] (a view's
//! stored query, a function body read for nullability, ...): those
//! references aren't the statement's own range table.
//!
//! Not modeled yet: `COPY`; foreign-key cascade actions (they run as the
//! table's owner).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

use crate::AnalyzeError;
use crate::ddl::acl::{AclObject, bits};
use crate::ddl::functions::RoutineSource;
use crate::ddl::policies::{Policy, PolicyRole};
use crate::ddl::roles::RoleId;
use crate::ddl::triggers::{
    TRIGGER_TYPE_BEFORE, TRIGGER_TYPE_DELETE, TRIGGER_TYPE_INSERT, TRIGGER_TYPE_UPDATE, Trigger,
};
use crate::error::RawError;
use crate::expr::ExprType;
use crate::oid::{PgClassOid, PgNamespaceOid, PgProcOid, PgTypeOid};
use crate::pg_catalog::{PG_CLASS_RELID, PgCatalog, ProKind, RelKind};
use typedpg_pg_query::ast::{self, Node};

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

/// A sequence and the privileges of which any one suffices to run the
/// sequence function that names it (`bits::USAGE | bits::UPDATE` for
/// `nextval`, ...).
pub(crate) type SequencePrivileges = (PgClassOid, u32);

/// What of a routine's body the planner of an EXPLAIN without ANALYZE still
/// reaches, or what simplifying an expression the planner then drops runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum PlannedBody {
    /// The planner inlines the body only while simplifying an expression it
    /// then drops — the argument of a STRICT call it replaces by NULL
    /// (`simplify_function` processes the arguments before
    /// `evaluate_function`'s NULL shortcut): what the inlined body's own
    /// constant folding runs, runs; the expression itself is never planned,
    /// so its relations aren't scanned and its routines never initialized.
    Simplified,
    /// The planner inlines the body into the plan (`inline_function` /
    /// `inline_set_returning_function`): its relations are planned, its
    /// routines initialized with the plan — no expression is evaluated.
    Inlined,
    /// The planner folds the call to a constant by running the body
    /// (`evaluate_function`: an IMMUTABLE call over constant arguments).
    Runs,
}

/// The site a routine is called from: an expression in a target list or
/// qual, or a `FROM` function scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallSite {
    Expression,
    From,
}

/// The command a relation is the target of, when it is the statement's
/// result relation: it decides which row-security policies apply
/// (`get_row_security_policies`' `commandType`, the statement's for the
/// result relation and SELECT for every other RTE).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetCommand {
    Insert,
    Update,
    Delete,
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
    /// The command the relation is the target of (`None`: it is only read;
    /// a write through a view maps onto its base relation).
    target: Option<TargetCommand>,
    /// `INSERT ... ON CONFLICT DO UPDATE`: the auxiliary UPDATE applies
    /// that command's policies too.
    conflict_update: bool,
    /// `INSERT ... ON CONFLICT DO SELECT` (PostgreSQL 19): the conflicting
    /// row it returns passes the SELECT policies, and the UPDATE ones'
    /// USING clauses when it locks the row.
    conflict_select: bool,
    /// A `MERGE` target: PG checks the INSERT, UPDATE and DELETE policies
    /// whichever action a row takes.
    merge_target: bool,
    /// The statement reads a column of its own DML target (a `WHERE`, a
    /// `SET` expression, RETURNING, an `ON CONFLICT` inference or a MERGE's
    /// `ON` condition): `requiredPerms` gets SELECT, and with it the
    /// relation's SELECT policies as WITH CHECK options and security quals
    /// (`get_row_security_policies`). The same relation read as a FROM item
    /// (`INSERT INTO t SELECT … FROM t`) does not set it: that occurrence is
    /// an RTE of its own, whose SELECT policies are security quals.
    target_select: bool,
    /// `checkAsUser` is a view's owner (as opposed to the policy's own
    /// role): the denial note names the view only then.
    view_owner: bool,
    /// The scans merged here are all certainly empty (the planner proves
    /// the relation a dummy one): their security quals are never built, so
    /// no policy expression runs. A DML target occurrence is never empty.
    proven_empty: bool,
    /// Named by a utility statement (`TRUNCATE`), which opens it outside
    /// any range table: no row-level security applies.
    no_range_table: bool,
    selected: Vec<Column>,
    inserted: Vec<Column>,
    updated: Vec<Column>,
    /// Locked by FOR UPDATE / SHARE: a view pushes the lock, and its
    /// UPDATE requirement, into its query's relations.
    locked: bool,
    /// The relation is read in FROM (a scan), rather than only being a
    /// DML target.
    scanned: bool,
    /// The writes the statement makes to it, whose triggers fire (moved
    /// onto the base relation when the write goes through a view).
    writes: Vec<WriteEvent>,
    /// The range table entry reads the relation with `inh = false`
    /// (`FROM ONLY t`): the rows of its inheritance children aren't
    /// reached, so a write through a view over it leaves them alone
    /// (`rewriteTargetView` keeps the flag).
    only: bool,
    /// The types of the values the statement assigns to the columns it
    /// writes (`None`: a `DEFAULT`, or an expression whose type the
    /// analyzer doesn't pin down). `coerce_type` leaves a value whose type
    /// already is the column's untouched, so its domain constraints don't
    /// run; any other value is coerced and runs them.
    assigned: Vec<(Column, Option<PgTypeOid>)>,
    /// The partitions an INSERT's rows are certainly routed to
    /// (`ExecFindPartition`, plan-time pruning): the planner initializes
    /// only their result relations.
    insert_leaves: Vec<PgClassOid>,
    /// A row whose partition isn't known: every partition may be reached.
    insert_route_unknown: bool,
}

/// A write a statement makes to a relation: the trigger event it fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WriteEvent {
    /// `TRIGGER_TYPE_INSERT` / `_UPDATE` / `_DELETE`.
    event: i32,
    /// `UPDATE ONLY` / `DELETE FROM ONLY`: the rows of inheritance children
    /// aren't reached (their row triggers don't fire).
    only: bool,
}

/// One output column of a view's stored query: its expression, and whether
/// the planner evaluates it when nothing reads the column.
#[derive(Debug, Clone, Default)]
pub(crate) struct ViewOutput {
    /// The planner keeps the output's evaluation: its expression may call a
    /// volatile function, return a set, or a sort / group / distinct-on /
    /// window clause names it.
    pub kept: bool,
    /// What the output's expression runs: its routines, sequences, sublinks.
    pub req: Requirements,
    /// The parse-time requirements the expression's *sublinks* delivered
    /// (marked by the query level that made them, see [`with_relation`]): a
    /// dropped output's subqueries are never planned, so these go with it.
    pub subplan: Requirements,
    /// What the parser recorded for the relations the output's own query
    /// level (or an enclosing one, through a correlated reference) reads:
    /// these `selectedCols` entries are checked by `ExecCheckPermissions`
    /// whatever the planner drops.
    pub parse_relations: Requirements,
}

/// A privilege a utility statement checks itself (`TRUNCATE`, `LOCK`,
/// `SET`, `REFRESH MATERIALIZED VIEW`): these open their relations outside
/// any range table, and check what their command needs.
#[derive(Debug, Clone)]
pub(crate) enum UtilityCheck {
    /// One of the privileges `any_of` (`bits::*`) on relation `relid`, held
    /// by `check_as` (`None`: the role running the statement) —
    /// `pg_class_aclcheck` with a mask (`truncate_check_perms`,
    /// `LockTableAclCheck`, `RangeVarCallbackMaintainsTable`).
    Relation {
        relid: PgClassOid,
        any_of: u32,
        check_as: Option<crate::ddl::roles::RoleId>,
    },
    /// Ownership of sequence `relid` (`TRUNCATE ... RESTART IDENTITY`'s
    /// `object_ownercheck`).
    SequenceOwner { relid: PgClassOid },
    /// `SET` of a parameter only a superuser may set unless granted
    /// (`PGC_SUSET`: `pg_parameter_aclcheck` for `ACL_SET`).
    SetParameter { name: String },
    /// `SET ROLE name`: the role must be able to become it
    /// (`member_can_set_role`).
    SetRole { name: String },
    /// `SET SESSION AUTHORIZATION name`: only a superuser session may
    /// become another role.
    SessionAuthorization { name: String },
    /// What a materialized view's stored query requires, checked as the
    /// view's owner (`REFRESH MATERIALIZED VIEW` switches to it).
    AsOwnerOf {
        relid: PgClassOid,
        req: Box<Requirements>,
    },
}

/// What reading a view runs: its stored query, split by the parts the
/// planner may drop (see [`crate::privileges`] module documentation).
#[derive(Debug, Clone, Default)]
pub(crate) struct ViewAccess {
    /// What the query runs whatever output columns are read (its FROM,
    /// WHERE, GROUP BY, HAVING, ...).
    pub shared: Requirements,
    /// One entry per output column, when the planner may drop an output
    /// nothing reads (a set operation's and a plain DISTINCT's are all
    /// kept, and `outputs` is empty then).
    pub outputs: Vec<ViewOutput>,
    /// Whether dropping unused outputs is certain: otherwise the whole
    /// query is in `shared`.
    pub removable: bool,
}

impl ViewAccess {
    /// Everything reading every column runs.
    pub(crate) fn all(&self) -> Requirements {
        let mut req = self.shared.clone();
        for output in &self.outputs {
            req.merge(&output.req);
            req.merge(&output.subplan);
            req.merge(&output.parse_relations);
        }
        req
    }

    /// What reading the output columns `used` runs (`None`: every one of
    /// them).
    pub(crate) fn reading(&self, used: Option<&HashSet<usize>>) -> Requirements {
        if !self.removable {
            return self.all();
        }
        let mut req = self.shared.clone();
        for (i, output) in self.outputs.iter().enumerate() {
            let read = used.is_none_or(|used| used.contains(&i));
            // The output's expression may be dropped, but the parser's
            // `selectedCols` on the relations its own level reads stays
            // (`ExecCheckPermissions` checks it): it is kept either way. The
            // subqueries inside the expression, and its own routines and
            // sequences, go with the output.
            req.merge(&output.parse_relations);
            if read || output.kept {
                req.merge(&output.req);
                req.merge(&output.subplan);
            }
        }
        req
    }
}

/// Add what a call of `oid` does to its body in an EXPLAIN without
/// ANALYZE: the strongest of the plans recorded for it.
fn merge_planned_body(list: &mut Vec<(PgProcOid, PlannedBody)>, oid: PgProcOid, plan: PlannedBody) {
    match list.iter_mut().find(|(o, _)| *o == oid) {
        Some((_, existing)) => *existing = (*existing).max(plan),
        None => list.push((oid, plan)),
    }
}

/// What a statement requires, in the order PG checks it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Requirements {
    schemas: Vec<PgNamespaceOid>,
    relations: Vec<RelationRequirement>,
    pub(crate) routines: Vec<(PgProcOid, RoutineKind)>,
    /// A sequence and the privileges of which any one suffices.
    pub(crate) sequences: Vec<SequencePrivileges>,
    /// Column defaults (or generation expressions) the statement fills,
    /// whose routines and sequences then run.
    defaults: Vec<(PgClassOid, i16)>,
    /// The routines the stored query of a view runs in its `WHERE` quals —
    /// what the rewriter keeps when a write goes through the view
    /// (`rewriteTargetView`): its select list isn't evaluated.
    qual_routines: Vec<(PgProcOid, RoutineKind)>,
    /// Likewise, the sequence functions its quals call.
    qual_sequences: Vec<SequencePrivileges>,
    /// Likewise, the domain coercions its quals evaluate.
    qual_domain_coercions: Vec<PgTypeOid>,
    /// Domain types the statement's expressions coerce values to
    /// (`CoerceToDomain`, explicit casts): their constraints run there.
    domain_coercions: Vec<PgTypeOid>,
    /// The routines whose bodies an EXPLAIN without ANALYZE still reaches:
    /// the planner inlines them into the plan, or folds the call to a
    /// constant by running it (see [`PlannedBody`]).
    planned_bodies: Vec<(PgProcOid, PlannedBody)>,
    /// The routines a parameter DEFAULT the statement expands runs (see
    /// [`crate::ddl::functions::RoutineDefault`]): the catalog binds the
    /// default's expression to them by OID when the routine is created, so
    /// `pg_depend` naming is not needed to keep them — unlike a name the
    /// analysis resolved again under the running role's search path.
    stable_routines: Vec<PgProcOid>,
    /// What a utility statement (`TRUNCATE`, `LOCK`, `SET`, `REFRESH`)
    /// checks itself, outside any range table.
    utility: Vec<UtilityCheck>,
    /// Whether the statement executes: EXPLAIN without ANALYZE starts its
    /// executor without running an expression, so nothing reaches a
    /// sequence's `nextval` and no trigger or routine body runs.
    executes: bool,
}

impl Requirements {
    /// A requirement set of a statement that runs.
    fn running() -> Requirements {
        Requirements {
            executes: true,
            ..Requirements::default()
        }
    }

    /// Add every requirement of `other` that isn't there already: what a
    /// part of a call's arguments needs, kept only when the call runs it.
    /// The relations the requirements name, in order.
    pub(crate) fn relation_oids(&self) -> Vec<PgClassOid> {
        let mut out: Vec<PgClassOid> = Vec::new();
        for rel in &self.relations {
            if !out.contains(&rel.relid) {
                out.push(rel.relid);
            }
        }
        out
    }

    fn merge(&mut self, other: &Requirements) {
        for &ns in &other.schemas {
            if !self.schemas.contains(&ns) {
                self.schemas.push(ns);
            }
        }
        self.utility.extend(other.utility.iter().cloned());
        for rel in &other.relations {
            let entry = match self
                .relations
                .iter_mut()
                .find(|r| r.relid == rel.relid && r.check_as == rel.check_as)
            {
                Some(entry) => entry,
                None => {
                    self.relations.push(rel.clone());
                    continue;
                }
            };
            entry.perms |= rel.perms;
            entry.locked |= rel.locked;
            entry.scanned |= rel.scanned;
            entry.only |= rel.only;
            entry.conflict_update |= rel.conflict_update;
            entry.conflict_select |= rel.conflict_select;
            entry.merge_target |= rel.merge_target;
            entry.target_select |= rel.target_select;
            entry.view_owner |= rel.view_owner;
            entry.no_range_table &= rel.no_range_table;
            // Every scan occurrence must be certainly empty: one live scan
            // (or a DML target) builds its security quals. A mark-only
            // occurrence (a column read on the relation's `selectedCols`)
            // is no range-table entry of its own, so it changes nothing.
            entry.proven_empty = if rel.target.is_some() || !rel.writes.is_empty() {
                false
            } else if !rel.scanned {
                entry.proven_empty
            } else if entry.target.is_some() || !entry.writes.is_empty() {
                false
            } else if entry.scanned {
                entry.proven_empty && rel.proven_empty
            } else {
                rel.proven_empty
            };
            entry.target = match (entry.target, rel.target) {
                // An INSERT ... ON CONFLICT DO UPDATE's UPDATE requirement
                // merges into the INSERT's: the statement is an INSERT.
                (Some(TargetCommand::Insert), Some(TargetCommand::Update)) => {
                    entry.conflict_update = true;
                    Some(TargetCommand::Insert)
                }
                (None, target) => target,
                (target, _) => target,
            };
            for (from, to) in [
                (&rel.selected, &mut entry.selected),
                (&rel.inserted, &mut entry.inserted),
                (&rel.updated, &mut entry.updated),
            ] {
                for c in from {
                    if !to.contains(c) {
                        to.push(c.clone());
                    }
                }
            }
            for assignment in &rel.assigned {
                if !entry.assigned.contains(assignment) {
                    entry.assigned.push(assignment.clone());
                }
            }
            for write in &rel.writes {
                if !entry.writes.contains(write) {
                    entry.writes.push(*write);
                }
            }
        }
        for &(oid, kind) in &other.routines {
            if !self.routines.iter().any(|(o, _)| *o == oid) {
                self.routines.push((oid, kind));
            }
        }
        for &(seq, any_of) in &other.sequences {
            if !self.sequences.contains(&(seq, any_of)) {
                self.sequences.push((seq, any_of));
            }
        }
        for &(relid, attnum) in &other.defaults {
            if !self.defaults.contains(&(relid, attnum)) {
                self.defaults.push((relid, attnum));
            }
        }
        for &ty in &other.domain_coercions {
            if !self.domain_coercions.contains(&ty) {
                self.domain_coercions.push(ty);
            }
        }
        for &(oid, plan) in &other.planned_bodies {
            merge_planned_body(&mut self.planned_bodies, oid, plan);
        }
        for &oid in &other.stable_routines {
            if !self.stable_routines.contains(&oid) {
                self.stable_routines.push(oid);
            }
        }
    }

    /// The routines the statement runs, in resolution order.
    pub(crate) fn routine_oids(&self) -> Vec<PgProcOid> {
        self.routines.iter().map(|(oid, _)| *oid).collect()
    }

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
                    target: None,
                    conflict_update: false,
                    conflict_select: false,
                    merge_target: false,
                    target_select: false,
                    view_owner: false,
                    proven_empty: false,
                    no_range_table: false,
                    selected: Vec::new(),
                    inserted: Vec::new(),
                    updated: Vec::new(),
                    locked: false,
                    scanned: false,
                    writes: Vec::new(),
                    only: false,
                    assigned: Vec::new(),
                    insert_leaves: Vec::new(),
                    insert_route_unknown: false,
                });
                self.relations.len() - 1
            }
        };
        &mut self.relations[i]
    }
}

thread_local! {
    static COLLECTOR: RefCell<Option<Requirements>> = const { RefCell::new(None) };
    /// The aliases of the FROM items the level being analysed certainly
    /// proves empty: the planner replaces them with a dummy relation, whose
    /// security quals are never built. A stack, one frame per level.
    static EMPTY_FROM_ALIASES: RefCell<Vec<Vec<String>>> = const { RefCell::new(Vec::new()) };
    /// Code the planner drops is still checked: a trigger's `WHEN`
    /// expression, which PG prepares whole (initializing every function in
    /// it, even one a short-circuit later skips).
    static ALL_CODE: Cell<u32> = const { Cell::new(0) };
    /// What the `WHERE` / `ON` clauses of the view being defined run: the
    /// rewriter keeps only those when a statement writes through the view
    /// (`rewriteTargetView`), its select list unevaluated.
    static VIEW_QUALS: RefCell<Option<Requirements>> = const { RefCell::new(None) };
    static SUSPENDED: Cell<u32> = const { Cell::new(0) };
    /// The role [`PgCatalog::analyze_as`] checks the statement against.
    static CHECKING: RefCell<Option<String>> = const { RefCell::new(None) };
    /// What the stored query of the view being defined reads, changes and
    /// runs. Unlike [`COLLECTOR`], it is active while [`SUSPENDED`] is: a
    /// view's dependencies are recorded with the statement collector off,
    /// but its query's access requirements must be kept for the statements
    /// that read the view (see [`PgCatalog::expand_views`]).
    static VIEW_COLLECTOR: RefCell<Option<Requirements>> = const { RefCell::new(None) };
    /// Code the planner folds away before execution (a dead CASE branch, a
    /// COALESCE argument after a constant): its relations, functions and
    /// sequences are never checked. Schemas are still resolved at parse
    /// time, so their USAGE keeps being noted.
    static NOT_EXECUTED: Cell<u32> = const { Cell::new(0) };
    /// The body being checked is planned, not run: an SQL function the
    /// planner inlines into an `EXPLAIN` without `ANALYZE` (see
    /// [`PlannedBody`]). The executor initializes its expressions — the
    /// plan's relations and routines are checked — but evaluates none.
    static PLANNED_BODY: Cell<u32> = const { Cell::new(0) };
    /// What is being checked is only simplified: the planner inlined it into
    /// an expression it then drops (see [`PlannedBody::Simplified`]). Only
    /// what the folding runs is checked — no relation, no routine's EXECUTE.
    static SIMPLIFIED_ONLY: Cell<u32> = const { Cell::new(0) };
    /// The statement being collected is only planned (`EXPLAIN` without
    /// `ANALYZE`, see [`crate::resolve::planned_only`]). Kept until the
    /// statement's collection ends: the level outputs it gathers are
    /// flushed after the planned analysis returns.
    static PLANNED_STATEMENT: Cell<bool> = const { Cell::new(false) };
}

/// Note that the statement being analyzed is planned only
/// ([`crate::resolve::planned_only`]): nothing it runs is evaluated.
pub(crate) fn note_planned_statement() {
    PLANNED_STATEMENT.with(|c| c.set(true));
    // The collectors were created before the analysis reached `EXPLAIN`, as
    // one of a running statement. The mode is the statement's, not a part's:
    // a requirement that reaches the collector from an output bucket (whose
    // own `executes` flag the `with` that delivers it set) must not leave
    // the collector "running" — `flush_output_group` merges buckets with
    // `Requirements::merge`, which carries no execution flag.
    for collector in [&COLLECTOR, &VIEW_COLLECTOR, &VIEW_QUALS] {
        collector.with(|c| {
            if let Some(req) = c.borrow_mut().as_mut() {
                req.executes = false;
            }
        });
    }
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

/// Run `f` collecting what a call argument requires, for a call that may not
/// evaluate it (an inlined SQL body drops the arguments it doesn't use).
/// `None` when no statement is being collected: nothing to keep.
pub(crate) fn collect_argument<R>(f: impl FnOnce() -> R) -> (R, Option<Requirements>) {
    if !active() {
        return (f(), None);
    }
    let (result, req) = collect(f);
    (result, Some(req))
}

/// Merge a [`collect_argument`]'s requirements into the statement's: all of
/// them when the call runs the argument, else what the planner runs while it
/// simplifies the argument before inlining drops it (see
/// [`keep_argument_simplified`]: `eval_const_expressions` simplifies a
/// call's arguments before `inline_function` substitutes them, so an
/// `IMMUTABLE` call over constants there is executed even when the inlined
/// body never uses the parameter).
pub(crate) fn keep_argument(req: Option<&Requirements>, runs: bool) {
    let Some(req) = req else {
        return;
    };
    if runs {
        with(|r| r.merge(req));
    } else {
        keep_argument_simplified(Some(req));
    }
}

/// [`keep_argument_simplified`] for a parameter DEFAULT expression the call
/// is expanded with but never evaluates (the inlined body ignores the
/// parameter, or a strict call folds to NULL): `expand_function_arguments`
/// inserts it before the arguments are simplified, so what its constant
/// folding runs, runs. The routines are the catalog's (bound by OID when the
/// function was created), so a stored expression's `pg_depend` filtering
/// keeps them (see [`note_default_routine`]).
pub(crate) fn keep_default_simplified(req: &Requirements) {
    keep_argument_simplified(Some(req));
    with(|r| {
        for &(oid, plan) in &req.planned_bodies {
            if plan == PlannedBody::Runs && !r.stable_routines.contains(&oid) {
                r.stable_routines.push(oid);
            }
        }
    });
}

/// Keep what the planner runs while it simplifies an argument of a call it
/// then replaces by NULL: `simplify_function` runs `eval_const_expressions`
/// over the arguments *before* `evaluate_function` applies a strict call's
/// NULL-argument shortcut, and an `IMMUTABLE` call over constant arguments
/// is executed there (it folds to a constant). That call's EXECUTE is
/// checked, and its body runs — in an `EXPLAIN` without `ANALYZE` too, as
/// the planner runs it. Everything else of the argument (a relation it
/// reads, a sequence, a call the planner doesn't fold) is never evaluated.
pub(crate) fn keep_argument_simplified(req: Option<&Requirements>) {
    let Some(req) = req else {
        return;
    };
    with_parse_time(|r| {
        for &ns in &req.schemas {
            if !r.schemas.contains(&ns) {
                r.schemas.push(ns);
            }
        }
    });
    for &(oid, plan) in &req.planned_bodies {
        match plan {
            // The call runs while the planner folds it: its EXECUTE and its
            // body.
            PlannedBody::Runs => with(|r| {
                if !r.routines.iter().any(|(o, _)| *o == oid) {
                    r.routines.push((oid, RoutineKind::Function));
                }
                merge_planned_body(&mut r.planned_bodies, oid, plan);
            }),
            // A call the planner inlines only to fold it: what its own
            // constant folding runs, runs; the inlined expression is then
            // dropped. Its EXECUTE is not checked (a refused EXECUTE keeps
            // the call from being inlined at all; the expression is dropped
            // then).
            PlannedBody::Inlined | PlannedBody::Simplified => with(|r| {
                merge_planned_body(&mut r.planned_bodies, oid, PlannedBody::Simplified);
            }),
        }
    }
}

/// Run `f` collecting the access requirements of the statement it
/// analyzes.
pub(crate) fn collect<R>(f: impl FnOnce() -> R) -> (R, Requirements) {
    /// Puts the enclosing collector and suspension back, however `f` ends:
    /// a panic must not leave this thread's state in the analysis.
    struct Restore {
        collector: Option<Requirements>,
        suspended: u32,
        bucket: Option<(usize, usize)>,
        planned: bool,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            SUSPENDED.with(|s| s.set(self.suspended));
            OUTPUT_BUCKET.with(|b| b.set(self.bucket));
            PLANNED_STATEMENT.with(|p| p.set(self.planned));
            COLLECTOR.with(|c| *c.borrow_mut() = self.collector.take());
        }
    }
    let collector = COLLECTOR.with(|c| {
        c.replace(Some(if PLANNED_STATEMENT.with(Cell::get) {
            // A nested collection of a statement only planned: what it gathers is
            // planned too (see [`note_planned_statement`]).
            Requirements {
                executes: false,
                ..Requirements::default()
            }
        } else {
            Requirements::running()
        }))
    });
    let suspended = SUSPENDED.with(|s| s.replace(0));
    // A nested collection (a call argument's, a body statement's) keeps
    // what it gathers, not the enclosing output column's part of it.
    let bucket = OUTPUT_BUCKET.with(|b| b.replace(None));
    // The statement's own planned-only mode starts here; the analysis sets
    // it when it reaches `EXPLAIN` (see [`note_planned_statement`]). A
    // nested collection (a call argument's, a body statement's) inside one
    // is planned with it.
    let planned = if collector.is_some() {
        PLANNED_STATEMENT.with(Cell::get)
    } else {
        PLANNED_STATEMENT.with(|p| p.replace(false))
    };
    let marker = level_output_marker();
    let _restore = Restore {
        collector,
        suspended,
        bucket,
        planned,
    };
    let result = f();
    // A level nothing derived reads (the statement's own SELECT) runs all
    // its outputs; a claimed relation's was flushed where it was resolved.
    flush_level_outputs_from(marker);
    let collected = COLLECTOR
        .with(|c| c.borrow_mut().take())
        .unwrap_or_default();
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

/// Run `f` collecting what the query of the view being defined reads,
/// changes and runs, for the statements that use the view: its clauses
/// (`shared`) and, when the planner may drop them, its output columns'
/// expressions.
pub(crate) fn collect_view<R>(f: impl FnOnce() -> R) -> (R, ViewAccess) {
    struct Restore(Option<Requirements>);
    impl Drop for Restore {
        fn drop(&mut self) {
            VIEW_COLLECTOR.with(|c| *c.borrow_mut() = self.0.take());
        }
    }
    let marker = level_output_marker();
    let outer = VIEW_COLLECTOR.with(|c| c.replace(Some(Requirements::default())));
    let _restore = Restore(outer);
    let result = f();
    let shared = VIEW_COLLECTOR.with(|c| c.replace(None)).unwrap_or_default();
    let outputs = take_view_outputs(marker)
        .map(|outputs| {
            outputs
                .into_iter()
                .map(|(kept, req, subplan, parse_relations)| ViewOutput {
                    kept,
                    req,
                    subplan,
                    parse_relations,
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let removable = !outputs.is_empty();
    (
        result,
        ViewAccess {
            shared,
            outputs,
            removable,
        },
    )
}

/// Run `f` collecting what the `WHERE` (and `ON`) clause of the view being
/// defined runs, kept with the view for the writes that go through it. Does
/// nothing outside a view's definition, nor inside an enclosing qual walk.
pub(crate) fn collect_view_qual<R>(f: impl FnOnce() -> R) -> R {
    if VIEW_QUALS.with(|c| c.borrow().is_some()) || VIEW_COLLECTOR.with(|c| c.borrow().is_none()) {
        return f();
    }
    /// Restores the enclosing qual collector however `f` ends.
    struct Restore(Option<Requirements>);
    impl Drop for Restore {
        fn drop(&mut self) {
            VIEW_QUALS.with(|c| *c.borrow_mut() = self.0.take());
        }
    }
    let outer = VIEW_QUALS.with(|c| c.replace(Some(Requirements::default())));
    let _restore = Restore(outer);
    let result = f();
    let collected = VIEW_QUALS
        .with(|c| c.borrow_mut().take())
        .unwrap_or_default();
    VIEW_COLLECTOR.with(|c| {
        if let Some(req) = c.borrow_mut().as_mut() {
            for (oid, kind) in collected.routines {
                if !req.qual_routines.iter().any(|(o, _)| *o == oid) {
                    req.qual_routines.push((oid, kind));
                }
            }
            for (seq, any_of) in collected.sequences {
                if !req.qual_sequences.contains(&(seq, any_of)) {
                    req.qual_sequences.push((seq, any_of));
                }
            }
            for ty in collected.domain_coercions {
                if !req.qual_domain_coercions.contains(&ty) {
                    req.qual_domain_coercions.push(ty);
                }
            }
        }
    });
    result
}

/// What one output column of an analyzed level evaluates, and whether the
/// planner evaluates it even when no reference reads it.
#[derive(Debug, Clone, Default)]
struct LevelOutput {
    /// The planner keeps the output's evaluation: its expression may call a
    /// volatile function, return a set, or a sort / group / distinct-on /
    /// window clause names it.
    kept: bool,
    /// What the output's expression runs.
    req: Requirements,
    /// What the expression's *sublinks* delivered: their relations are only
    /// planned when the output is read (`remove_unused_subquery_outputs`
    /// drops the whole expression).
    subplan: Requirements,
    /// The parse-time column and relation marks on the relations the
    /// output's own query level (or an enclosing one, through a correlated
    /// reference) reads: `ExecCheckPermissions` checks them even when the
    /// planner drops the output.
    parse_relations: Requirements,
}

/// One analyzed level's output buckets, kept while the relation it feeds is
/// resolved: a FROM subquery's, a CTE body's, a view's stored query. The
/// planner drops an output nothing reads (`pull_up_simple_subquery` /
/// `remove_unused_subquery_outputs`), so its expression runs no privilege
/// check.
#[derive(Debug, Clone, Default)]
struct LevelOutputs {
    outputs: Vec<LevelOutput>,
    used: Vec<bool>,
    /// The level feeds a derived relation (a FROM subquery, an inlined
    /// CTE, a view's query): only the outputs read run.
    claimed: bool,
    /// Every output is kept: a column cannot be dropped soundly (an arm the
    /// analyzer can't attribute per output, a UNION ALL tree whose types
    /// coerce).
    keep_all: bool,
}

thread_local! {
    /// Every level's output buckets, by id (`None`: flushed or taken).
    /// Ids are never reused, so a relation may keep its id.
    static LEVEL_OUTPUTS: RefCell<Vec<Option<LevelOutputs>>> =
        const { RefCell::new(Vec::new()) };
    /// The levels being resolved, outermost first: the group id of each
    /// (`None`: its outputs aren't tracked).
    static OPEN_LEVELS: RefCell<Vec<Option<usize>>> = const { RefCell::new(Vec::new()) };
    /// The output bucket currently receiving requirements: the level's
    /// group and the output column being resolved.
    static OUTPUT_BUCKET: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
    /// The group the level analyzed last created, for the relation reading
    /// it (a FROM subquery, a CTE body, a view's query).
    static LAST_LEVEL_OUTPUTS: Cell<Option<usize>> = const { Cell::new(None) };
    /// Depth inside a set operation, whose arms' outputs the planner keeps
    /// (`remove_unused_subquery_outputs` stops at `setOperations`).
    static NO_OUTPUT_TRACKING: Cell<u32> = const { Cell::new(0) };
}

/// Whether a level's outputs can be tracked: a statement (or a view's
/// query) is being collected, nothing suspends it, and no output bucket is
/// already active (a level whose outputs run only with an outer one's).
fn tracks_outputs() -> bool {
    SUSPENDED.with(Cell::get) == 0
        && OUTPUT_BUCKET.with(Cell::get).is_none()
        && (VIEW_COLLECTOR.with(|c| c.borrow().is_some())
            || COLLECTOR.with(|c| c.borrow().is_some()))
}

/// The output columns `sel` may drop when nothing reads them; `None` when
/// the planner keeps every output (`remove_unused_subquery_outputs` bails
/// out for a set operation and a plain DISTINCT; a VALUES list's expressions
/// are not a subquery's target list; a star is not attributed per column).
fn level_output_plan(
    sel: &typedpg_pg_query::ast::SelectStmt,
    snapshot: &PgCatalog,
) -> Option<Vec<bool>> {
    use typedpg_pg_query::ast::Node;
    // A set operation's outputs are kept (`subquery->setOperations`), and a
    // VALUES subquery's expressions live in its arms, not the target list.
    if sel.op != typedpg_pg_query::ast::SetOperation::SetopNone || !sel.values_lists.is_empty() {
        return None;
    }
    // A plain DISTINCT uses every output column in its distinctClause
    // (`!hasDistinctOn`); DISTINCT ON's keys keep the outputs they name.
    let plain_distinct =
        !sel.distinct_clause.is_empty() && sel.distinct_clause.iter().all(|n| n.is_none());
    if plain_distinct {
        return None;
    }
    let named = outputs_named_by_keys(sel)?;
    let mut kept = Vec::with_capacity(sel.target_list.len());
    for (i, target) in sel.target_list.iter().enumerate() {
        let Some(Node::ResTarget(rt)) = target.as_ref() else {
            return None;
        };
        // A star expands to several outputs, which no ResTarget stands for.
        let star = match rt.val.as_ref() {
            Some(Node::ColumnRef(cr)) => cr
                .fields
                .iter()
                .any(|f| matches!(f.as_ref(), Some(Node::AStar(_)))),
            Some(Node::AIndirection(_)) => true,
            _ => false,
        };
        if star {
            return None;
        }
        // The output keeps a `ressortgroupref` when a GROUP BY / ORDER BY /
        // DISTINCT ON / window key names it; a volatile expression and a
        // set-returning one are kept too (`contain_volatile_functions`,
        // `hasTargetSRFs && expression_returns_set`).
        let mut keep = named[i];
        if let Some(expr) = rt.val.as_ref() {
            keep |= crate::expr::plan_fold::may_be_volatile(Some(expr), snapshot);
            keep |= contains_set_returning_call(Some(expr), snapshot);
        }
        kept.push(keep);
    }
    Some(kept)
}

/// The output columns a sort, group, distinct-on or window clause names,
/// one `bool` per target entry: PG marks the matching target entry with a
/// `ressortgroupref`, and `remove_unused_subquery_outputs` keeps it.
///
/// The raw tree carries no refs: they are recomputed here the way PG's
/// `findTargetlistEntrySQL92` / `findTargetlistEntrySQL99` attach them. A
/// key the analyzer cannot certainly match marks nothing: the key's own
/// expression is analyzed in the level's shared requirements anyway, so a
/// missed match only keeps more checks, never fewer.
fn outputs_named_by_keys(sel: &typedpg_pg_query::ast::SelectStmt) -> Option<Vec<bool>> {
    use typedpg_pg_query::ast::Node;
    let mut named = vec![false; sel.target_list.len()];
    let targets: Vec<&typedpg_pg_query::ast::ResTarget> = sel
        .target_list
        .iter()
        .map(|t| match t.as_ref()? {
            Node::ResTarget(rt) => Some(&**rt),
            _ => None,
        })
        .collect::<Option<_>>()?;
    // ORDER BY / GROUP BY / DISTINCT ON resolve a bare name or an integer
    // against the projection (`findTargetlistEntrySQL92`); a window's keys
    // use SQL99 rules only (`useSQL99` in transformWindowDefinitions).
    let mut mark = |node: Option<&typedpg_pg_query::ast::Node>, sql92: bool| {
        mark_key_targets(node, sql92, &targets, &mut named);
    };
    for key in &sel.group_clause {
        group_clause_keys(key.as_ref(), &mut |key| mark(key, true));
    }
    for sort in &sel.sort_clause {
        if let Some(Node::SortBy(sb)) = sort.as_ref()
            && let Some(inner) = sb.node.as_ref()
        {
            mark(Some(inner), true);
        }
    }
    for distinct in &sel.distinct_clause {
        if distinct.is_some() {
            mark(distinct.as_ref(), true);
        }
    }
    // The WINDOW clause's definitions, and the inline `OVER (…)` of every
    // window call at this level (transformWindowDefinitions processes the
    // target list's, then ORDER BY's, then DISTINCT ON's).
    for window in &sel.window_clause {
        if let Some(Node::WindowDef(wd)) = window.as_ref() {
            mark_window_def(wd, &targets, &mut named);
        }
    }
    let mut exprs: Vec<&typedpg_pg_query::ast::Node> = Vec::new();
    for target in &sel.target_list {
        if let Some(Node::ResTarget(rt)) = target.as_ref()
            && let Some(val) = rt.val.as_ref()
        {
            exprs.push(val);
        }
    }
    for sort in &sel.sort_clause {
        if let Some(Node::SortBy(sb)) = sort.as_ref()
            && let Some(inner) = sb.node.as_ref()
        {
            exprs.push(inner);
        }
    }
    exprs.extend(sel.distinct_clause.iter().flatten());
    for expr in exprs {
        let Some(inner) = Some(expr) else {
            continue;
        };
        let mut todo = vec![inner.to_ref()];
        while let Some(n) = todo.pop() {
            use typedpg_pg_query::NodeRef;
            let window = match n {
                NodeRef::FuncCall(fc) => fc.over.as_deref(),
                NodeRef::JsonObjectAgg(a) => {
                    a.constructor.as_deref().and_then(|c| c.over.as_deref())
                }
                NodeRef::JsonArrayAgg(a) => {
                    a.constructor.as_deref().and_then(|c| c.over.as_deref())
                }
                // A subquery is another level: its windows name its own
                // outputs.
                NodeRef::SubLink(_) => continue,
                _ => None,
            };
            if let Some(wd) = window {
                mark_window_def(wd, &targets, &mut named);
                continue;
            }
            todo.extend(n.children());
        }
    }
    Some(named)
}

/// The expressions of one `GROUP BY` entry: a `GROUPING SETS` /
/// `ROLLUP` / `CUBE` node's members and an implicit row's items are
/// grouping keys of their own.
fn group_clause_keys<'a>(
    node: Option<&'a typedpg_pg_query::ast::Node>,
    mark: &mut dyn FnMut(Option<&'a typedpg_pg_query::ast::Node>),
) {
    use typedpg_pg_query::ast::Node;
    let members = match node {
        Some(Node::GroupingSet(gs)) => Some(&gs.content),
        _ => crate::grouping::implicit_row_items(node),
    };
    match members {
        Some(members) => {
            for inner in members {
                group_clause_keys(inner.as_ref(), mark);
            }
        }
        None => mark(node),
    }
}

/// Mark the target entries a window definition's PARTITION BY / ORDER BY
/// keys name by expression (`findTargetlistEntrySQL99`: no ordinals, no
/// output aliases).
fn mark_window_def(
    wd: &typedpg_pg_query::ast::WindowDef,
    targets: &[&typedpg_pg_query::ast::ResTarget],
    named: &mut [bool],
) {
    use typedpg_pg_query::ast::Node;
    for key in wd.partition_clause.iter().chain(wd.order_clause.iter()) {
        let inner = match key.as_ref() {
            Some(Node::SortBy(sb)) => sb.node.as_ref(),
            _ => key.as_ref(),
        };
        if let Some(inner) = inner {
            mark_key_targets(Some(inner), false, targets, named);
        }
    }
}

/// Mark the target entries `key` names: an integer ordinal or a bare output
/// name (`sql92`), or an expression structurally equal to a target's
/// (SQL99).
fn mark_key_targets(
    key: Option<&typedpg_pg_query::ast::Node>,
    sql92: bool,
    targets: &[&typedpg_pg_query::ast::ResTarget],
    named: &mut [bool],
) {
    use typedpg_pg_query::ast::Node;
    if sql92 {
        // `findTargetlistEntrySQL92`: an integer constant is a 1-based
        // projection ordinal.
        if let Some(Node::AConst(ac)) = key
            && !ac.isnull
            && let Some(typedpg_pg_query::ast::AConstValue::Integer(i)) = &ac.val
            && i.ival >= 1
            && let Some(slot) = named.get_mut(i.ival as usize - 1)
        {
            *slot = true;
            return;
        }
        // A bare identifier names an output column of that name. For GROUP
        // BY PG prefers a FROM column, which is an expression key then — a
        // target matched here anyway was matched by its expression below
        // too, so keeping it can only over-check.
        if let Some(Node::ColumnRef(cr)) = key
            && let [field] = cr.fields.as_slice()
            && let Some(Node::String(name)) = field.as_ref()
        {
            let mut matched = false;
            for (i, rt) in targets.iter().enumerate() {
                // PG folds unquoted identifiers; a quoted one that differs
                // only in case cannot be told apart here, and matching it
                // keeps an output PG may drop.
                if crate::resolve::target_resname(rt).eq_ignore_ascii_case(&name.sval) {
                    if let Some(slot) = named.get_mut(i) {
                        *slot = true;
                    }
                    matched = true;
                }
            }
            if matched {
                return;
            }
        }
    }
    let want = crate::resolve::node_fingerprint(key);
    for (i, rt) in targets.iter().enumerate() {
        if rt
            .val
            .as_ref()
            .is_some_and(|val| crate::resolve::node_fingerprint(Some(val)) == want)
            && let Some(slot) = named.get_mut(i)
        {
            *slot = true;
        }
    }
}

/// Whether `node` calls a set-returning function at its own query level
/// (`expression_returns_set`): the planner keeps such an output.
fn contains_set_returning_call(
    node: Option<&typedpg_pg_query::ast::Node>,
    snapshot: &PgCatalog,
) -> bool {
    use typedpg_pg_query::NodeRef;
    let Some(inner) = node else {
        return false;
    };
    let mut todo = vec![inner.to_ref()];
    while let Some(n) = todo.pop() {
        match n {
            NodeRef::FuncCall(fc) => {
                if crate::resolve::walk::is_srf_call(fc, snapshot) {
                    return true;
                }
            }
            // A sublink's query is another level; its own outputs are
            // handled when it is analyzed.
            NodeRef::SubLink(_) => continue,
            _ => {}
        }
        todo.extend(n.children());
    }
    false
}

/// The guard of one analyzed level's output tracking.
pub(crate) struct LevelOutputsGuard {
    group: Option<usize>,
    marker: usize,
    /// Whether the level disabled output tracking for its arms (a set
    /// operation other than a simple `UNION ALL` tree), which it re-enables
    /// on drop.
    untracked_arms: bool,
    /// Whether the level created a simple `UNION ALL` tree's group, which it
    /// pops on drop.
    own_union_all_group: bool,
}

thread_local! {
    /// Whether each query level being analyzed, outermost first, is a
    /// sublink's inside an output expression. A relation mark carries the
    /// flag of the level whose parse made it: a mark made by a sublink's
    /// level belongs to the output's expression (and is dropped with it),
    /// one the level's own parse (or an enclosing level's, through a
    /// correlated reference) made is the level's `perminfo`.
    static OPEN_LEVEL_SUBLINKS: RefCell<Vec<bool>> = const { RefCell::new(Vec::new()) };
    /// The output group a simple `UNION ALL` tree's arms deliver into: the
    /// planner pulls the tree up (`pull_up_simple_union_all`) into an append
    /// relation, and each leaf's unused outputs are dropped. `Some(id)` while
    /// such a tree is analyzed.
    static UNION_ALL_GROUPS: RefCell<Vec<Option<usize>>> = const { RefCell::new(Vec::new()) };
}

/// Whether the innermost query level being analyzed is a sublink's inside an
/// output expression, if any.
pub(crate) fn current_level_is_sublink() -> bool {
    OPEN_LEVEL_SUBLINKS.with(|levels| levels.borrow().last().copied().unwrap_or(false))
}

impl LevelOutputsGuard {
    /// Start tracking the outputs of the level about to be analyzed, when
    /// the planner may drop them.
    pub(crate) fn enter(sel: &typedpg_pg_query::ast::SelectStmt, snapshot: &PgCatalog) -> Self {
        let setop = sel.op != typedpg_pg_query::ast::SetOperation::SetopNone;
        let sublink = OUTPUT_BUCKET.with(Cell::get).is_some();
        OPEN_LEVEL_SUBLINKS.with(|levels| levels.borrow_mut().push(sublink));
        let shared = UNION_ALL_GROUPS.with(|groups| groups.borrow().last().copied().flatten());
        let marker = LEVEL_OUTPUTS.with(|r| r.borrow().len());
        let mut kept = None;
        let mut union_all_group = None;
        let mut own_union_all_group = false;
        if NO_OUTPUT_TRACKING.with(Cell::get) == 0 && tracks_outputs() {
            if setop {
                if let Some(shared) = shared {
                    // A nested node of a simple `UNION ALL` tree: its leaves
                    // deliver into the tree's group.
                    union_all_group = Some(shared);
                } else if simple_union_all(sel) {
                    // `pull_up_simple_union_all`: the tree becomes an append
                    // relation, and each leaf's unused outputs are dropped.
                    LEVEL_OUTPUTS.with(|r| {
                        r.borrow_mut().push(Some(LevelOutputs {
                            outputs: Vec::new(),
                            used: Vec::new(),
                            claimed: false,
                            keep_all: false,
                        }));
                    });
                    UNION_ALL_GROUPS.with(|groups| groups.borrow_mut().push(Some(marker)));
                    union_all_group = Some(marker);
                    own_union_all_group = true;
                }
            } else if let Some(shared) = shared {
                // A leaf of the tree: its kept outputs (`remove_unused_
                // subquery_outputs`' rules) join the tree's group, and its
                // expressions deliver into it.
                match level_output_plan(sel, snapshot) {
                    Some(kept) => merge_kept_into(shared, &kept),
                    // A star, a plain DISTINCT or a VALUES arm: not
                    // attributable per output, keep them all.
                    None => mark_keep_all(shared),
                }
                union_all_group = Some(shared);
            } else {
                kept = level_output_plan(sel, snapshot);
            }
        }
        // The arms of any other set operation are levels of their own, and
        // their outputs are all kept.
        let untracked_arms = setop && union_all_group.is_none();
        if untracked_arms {
            NO_OUTPUT_TRACKING.with(|c| c.set(c.get() + 1));
        }
        let group = match (kept, union_all_group) {
            (_, Some(shared)) => Some(shared),
            (Some(kept), None) => {
                let outputs = kept
                    .iter()
                    .map(|&kept| LevelOutput {
                        kept,
                        ..LevelOutput::default()
                    })
                    .collect::<Vec<_>>();
                let used = vec![false; outputs.len()];
                LEVEL_OUTPUTS.with(|r| {
                    r.borrow_mut().push(Some(LevelOutputs {
                        outputs,
                        used,
                        claimed: false,
                        keep_all: false,
                    }));
                });
                Some(marker)
            }
            (None, None) => None,
        };
        OPEN_LEVELS.with(|levels| levels.borrow_mut().push(group));
        LAST_LEVEL_OUTPUTS.with(|c| c.set(None));
        LevelOutputsGuard {
            group,
            marker,
            untracked_arms,
            own_union_all_group,
        }
    }
}

impl Drop for LevelOutputsGuard {
    fn drop(&mut self) {
        OPEN_LEVEL_SUBLINKS.with(|levels| {
            levels.borrow_mut().pop();
        });
        OPEN_LEVELS.with(|levels| {
            levels.borrow_mut().pop();
        });
        // What no relation claimed is a level that always runs its outputs:
        // a scalar sublink's, a materialized CTE's, the statement's own.
        let ids: Vec<usize> = LEVEL_OUTPUTS.with(|r| {
            let registry = r.borrow();
            (self.marker + 1..registry.len())
                .filter(|&i| registry[i].is_some())
                .collect()
        });
        for id in ids {
            flush_output_group(id);
        }
        if self.untracked_arms {
            NO_OUTPUT_TRACKING.with(|c| c.set(c.get() - 1));
        }
        if self.own_union_all_group {
            UNION_ALL_GROUPS.with(|groups| {
                groups.borrow_mut().pop();
            });
        }
        // The level's own group is claimed by the relation reading it; one
        // nothing reads is flushed by the enclosing level.
        LAST_LEVEL_OUTPUTS.with(|c| c.set(self.group));
    }
}

/// Merge the level group `id` into the current sink: the outputs something
/// read or the planner keeps when a relation claimed it, else all of them.
/// A bucket's schema USAGE was resolved at parse time, before any dropping,
/// so it is always kept.
fn flush_output_group(id: usize) {
    let Some(group) = LEVEL_OUTPUTS.with(|r| r.borrow_mut()[id].take()) else {
        return;
    };
    for output in &group.outputs {
        let schemas: Vec<PgNamespaceOid> = output
            .req
            .schemas
            .iter()
            .chain(output.subplan.schemas.iter())
            .chain(output.parse_relations.schemas.iter())
            .copied()
            .collect();
        with_parse_time(|req| {
            for ns in schemas {
                if !req.schemas.contains(&ns) {
                    req.schemas.push(ns);
                }
            }
        });
    }
    for (i, output) in group.outputs.iter().enumerate() {
        // A dropped output's expression is never evaluated: its routines,
        // sequences and sublink relations go. The column marks the parser
        // recorded for the level's own relations stay (`selectedCols` is
        // checked by `ExecCheckPermissions` whatever the planner dropped).
        let dropped = group.claimed
            && !group.keep_all
            && !output.kept
            && !group.used.get(i).copied().unwrap_or(false);
        with(|req| {
            req.merge(&output.parse_relations);
            if !dropped {
                req.merge(&output.req);
                req.merge(&output.subplan);
            }
        });
    }
}

/// A column of a derived relation (a FROM subquery's, an inlined CTE's):
/// output `index` of the level group `group`, whose expression runs only
/// when something reads the column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutputRef {
    pub group: usize,
    pub index: usize,
}

/// Mark output `index` of the group `group` read: only its expression runs.
pub(crate) fn note_output_used(group: usize, index: usize) {
    LEVEL_OUTPUTS.with(|r| {
        if let Some(Some(g)) = r.borrow_mut().get_mut(group)
            && index < g.used.len()
        {
            g.used[index] = true;
        }
    });
}

/// The group the level analyzed last created, for the relation reading it.
/// `None`: the level's outputs aren't tracked (all of them run).
pub(crate) fn take_level_outputs() -> Option<usize> {
    let id = LAST_LEVEL_OUTPUTS.with(Cell::take)?;
    let mut claimed = false;
    LEVEL_OUTPUTS.with(|r| {
        if let Some(Some(g)) = r.borrow_mut().get_mut(id) {
            g.claimed = true;
            claimed = true;
        }
    });
    claimed.then_some(id)
}

/// The number of level groups registered so far (a view's marker).
pub(crate) fn level_output_marker() -> usize {
    LEVEL_OUTPUTS.with(|r| r.borrow().len())
}

/// Take the buckets of the level a view's stored query analyzed, if the
/// planner may drop any of them: the group at or after `marker`, which no
/// relation claimed.
pub(crate) fn take_view_outputs(
    marker: usize,
) -> Option<Vec<(bool, Requirements, Requirements, Requirements)>> {
    let id = LEVEL_OUTPUTS.with(|r| {
        let registry = r.borrow();
        (marker..registry.len()).find(|&i| registry[i].as_ref().is_some_and(|g| !g.claimed))
    })?;
    let group = LEVEL_OUTPUTS.with(|r| r.borrow_mut()[id].take())?;
    let keep_all = group.keep_all;
    Some(
        group
            .outputs
            .into_iter()
            .map(|o| (o.kept || keep_all, o.req, o.subplan, o.parse_relations))
            .collect(),
    )
}

/// Flush every group registered at or after `marker`, for the statement
/// collection ending: no relation claimed them (a top-level SELECT), so all
/// their outputs run.
pub(crate) fn flush_level_outputs_from(marker: usize) {
    let ids: Vec<usize> = LEVEL_OUTPUTS.with(|r| {
        let registry = r.borrow();
        (marker..registry.len())
            .filter(|&i| registry[i].is_some())
            .collect()
    });
    for id in ids {
        flush_output_group(id);
    }
}

/// Run `f` resolving output column `index` of the innermost level: what its
/// expression runs is kept apart from the rest of the level's query, so a
/// read of another output drops it.
pub(crate) fn collect_output<R>(index: usize, f: impl FnOnce() -> R) -> R {
    if SUSPENDED.with(Cell::get) > 0
        || NOT_EXECUTED.with(Cell::get) > 0
        || OUTPUT_BUCKET.with(Cell::get).is_some()
    {
        return f();
    }
    let Some(Some(group)) = OPEN_LEVELS.with(|l| l.borrow().last().copied()) else {
        return f();
    };
    let prepared = LEVEL_OUTPUTS.with(|r| {
        let mut registry = r.borrow_mut();
        let Some(Some(g)) = registry.get_mut(group) else {
            return false;
        };
        while g.outputs.len() <= index {
            g.outputs.push(LevelOutput::default());
            g.used.push(false);
        }
        true
    });
    if !prepared {
        return f();
    }
    struct Restore(Option<(usize, usize)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            OUTPUT_BUCKET.with(|c| c.set(self.0.take()));
        }
    }
    let outer = OUTPUT_BUCKET.with(|c| c.replace(Some((group, index))));
    let _restore = Restore(outer);
    f()
}

/// Deliver `f`'s requirements to the active output bucket's `part`.
fn deliver_to_bucket<F: FnMut(&mut Requirements)>(part: BucketPart, mut f: F) {
    let Some((group, index)) = OUTPUT_BUCKET.with(Cell::get) else {
        return;
    };
    LEVEL_OUTPUTS.with(|r| {
        if let Some(Some(g)) = r.borrow_mut().get_mut(group) {
            while g.outputs.len() <= index {
                g.outputs.push(LevelOutput::default());
                g.used.push(false);
            }
            let output = &mut g.outputs[index];
            let req = match part {
                BucketPart::Main => &mut output.req,
                BucketPart::Subplan => &mut output.subplan,
                BucketPart::ParseRelations => &mut output.parse_relations,
            };
            f(req);
            mark_execution(req);
        }
    });
}

/// Run `f` with no code considered dropped: what PG initializes even when
/// it never evaluates it (a trigger's `WHEN` expression).
pub(crate) fn all_code_checked<R>(f: impl FnOnce() -> R) -> R {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            ALL_CODE.with(|c| c.set(c.get() - 1));
        }
    }
    ALL_CODE.with(|c| c.set(c.get() + 1));
    let _restore = Restore;
    f()
}

/// Whether dropped code is still checked (see [`all_code_checked`]).
pub(crate) fn checks_dropped_code() -> bool {
    ALL_CODE.with(Cell::get) > 0
}

/// Run `f` for code the planner folds away: only what execution would check
/// (relations' privileges, functions' EXECUTE, sequences) is dropped. Name
/// resolution — a schema's USAGE, a relation's existence — happened before
/// the planner ran, so it is still noted.
pub(crate) fn not_executed<R>(f: impl FnOnce() -> R) -> R {
    struct Resume;
    impl Drop for Resume {
        fn drop(&mut self) {
            NOT_EXECUTED.with(|s| s.set(s.get() - 1));
        }
    }
    NOT_EXECUTED.with(|s| s.set(s.get() + 1));
    let _resume = Resume;
    f()
}

/// Which part of an output bucket a requirement goes to: the expression's
/// own (`Main`), or a subquery's inside it (`Subplan`), which a dropped
/// output's plan never reaches.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BucketPart {
    /// The output expression's own droppable requirements.
    Main,
    /// A subquery's inside the expression (dropped with the output).
    Subplan,
    /// A relation mark the parser recorded (kept whatever the planner picks).
    ParseRelations,
}

fn with<F: FnMut(&mut Requirements)>(f: F) {
    // A requirement delivered while a sublink's level is being analyzed
    // belongs to a subquery inside the output: its plan is only reached when
    // the output is read.
    let part = if OUTPUT_BUCKET.with(Cell::get).is_some() && current_level_is_sublink() {
        BucketPart::Subplan
    } else {
        BucketPart::Main
    };
    deliver(part, f);
}

/// Like [`with`], for a requirement of a relation whose reference was parsed
/// by query level `level`: when the requirement is delivered to an output
/// bucket of a level entered *earlier*, the relation belongs to a subquery
/// inside that output (a [sub]link), and its plan is only reached when the
/// output is read. A mark on a relation of the bucket's own level (or an
/// outer one — a correlated reference) is that level's `perminfo`, which the
/// parser recorded whatever the planner drops.
fn with_relation<F: FnMut(&mut Requirements)>(sublink: bool, f: F) {
    let part = if OUTPUT_BUCKET.with(Cell::get).is_some() {
        if sublink {
            BucketPart::Subplan
        } else {
            BucketPart::ParseRelations
        }
    } else {
        BucketPart::Main
    };
    deliver(part, f);
}

fn deliver<F: FnMut(&mut Requirements)>(part: BucketPart, mut f: F) {
    if NOT_EXECUTED.with(Cell::get) > 0 {
        return;
    }
    // An internal analysis — a stored query read for its shape, a function
    // body read for nullability — belongs to no statement and no view.
    if SUSPENDED.with(Cell::get) > 0 {
        return;
    }
    // An output column's expression: what it runs is kept apart, for the
    // planner drops the output when nothing reads it.
    if OUTPUT_BUCKET.with(Cell::get).is_some() {
        deliver_to_bucket(part, &mut f);
        return;
    }
    deliver_with(f);
}

/// Mark `req` as one of a statement whose executor only starts: EXPLAIN
/// without ANALYZE runs no expression, so nothing reaches a sequence, a
/// trigger or a routine body.
fn mark_execution(req: &mut Requirements) {
    if PLANNED_STATEMENT.with(Cell::get) || PLANNED_BODY.with(Cell::get) > 0 {
        req.executes = false;
    }
}

/// The guard of [`PLANNED_BODY`]: an inlined SQL body the planner only
/// plans.
struct PlannedBodyGuard;

impl PlannedBodyGuard {
    fn enter() -> Self {
        PLANNED_BODY.with(|c| c.set(c.get() + 1));
        PlannedBodyGuard
    }
}

impl Drop for PlannedBodyGuard {
    fn drop(&mut self) {
        PLANNED_BODY.with(|c| c.set(c.get() - 1));
    }
}

/// The guard of [`SIMPLIFIED_ONLY`]: an expression only simplified, or a
/// body inlined into one. Saved and restored, so a body a simplified
/// expression *runs* gets checked whole again.
struct SimplifiedGuard(u32);

impl SimplifiedGuard {
    fn set(on: bool) -> Self {
        let previous = SIMPLIFIED_ONLY.with(Cell::get);
        SIMPLIFIED_ONLY.with(|c| c.set(if on { 1 } else { 0 }));
        SimplifiedGuard(previous)
    }
}

impl Drop for SimplifiedGuard {
    fn drop(&mut self) {
        SIMPLIFIED_ONLY.with(|c| c.set(self.0));
    }
}

/// [`with`]'s delivery to the collectors (no output bucket active).
fn deliver_with<F: FnMut(&mut Requirements)>(mut f: F) {
    // A nested collection — a call argument's, a body statement's — has
    // replaced the statement collector with its own, even while a view's
    // query is being defined: what it gathers is kept apart for the nested
    // analysis to keep or drop (see [`collect_argument`]).
    if deliver_to_collector(&mut f) {
        return;
    }
    // The query of a view being defined is analyzed with the statement
    // collector off; what it reads is kept for the statements reading the
    // view. Its quals go to both collectors: the whole query, and the part
    // a write through the view evaluates.
    VIEW_COLLECTOR.with(|c| {
        if let Some(req) = c.borrow_mut().as_mut() {
            f(req);
            mark_execution(req);
        }
    });
    VIEW_QUALS.with(|c| {
        if let Some(req) = c.borrow_mut().as_mut() {
            f(req);
            mark_execution(req);
        }
    });
}

/// Deliver to the active [`COLLECTOR`], if any: the statement's own, or a
/// nested collection's that has replaced it.
fn deliver_to_collector<F: FnMut(&mut Requirements)>(f: &mut F) -> bool {
    COLLECTOR.with(|c| {
        if let Some(req) = c.borrow_mut().as_mut() {
            f(req);
            mark_execution(req);
            true
        } else {
            false
        }
    })
}

/// Like [`with`], for what is checked while the statement is parsed:
/// schemas' USAGE survives the planner's folding.
fn with_parse_time<F: FnOnce(&mut Requirements)>(f: F) {
    if SUSPENDED.with(Cell::get) > 0 {
        return;
    }
    let mut f = Some(f);
    if OUTPUT_BUCKET.with(Cell::get).is_some() {
        deliver_to_bucket(BucketPart::Main, |req| f.take().expect("called once")(req));
        return;
    }
    let mut delivered = false;
    COLLECTOR.with(|c| {
        if let Some(req) = c.borrow_mut().as_mut() {
            f.take().expect("called once")(req);
            mark_execution(req);
            delivered = true;
        }
    });
    if delivered {
        return;
    }
    let f = f.expect("not delivered");
    VIEW_COLLECTOR.with(|c| {
        if let Some(req) = c.borrow_mut().as_mut() {
            f(req);
            mark_execution(req);
        }
    });
}

/// Whether a statement's requirements are being collected.
fn active() -> bool {
    NOT_EXECUTED.with(Cell::get) == 0
        && SUSPENDED.with(Cell::get) == 0
        && (VIEW_COLLECTOR.with(|c| c.borrow().is_some())
            || COLLECTOR.with(|c| c.borrow().is_some()))
}

/// The statement's range table has relation `relid`, which requires
/// `perms` (`addRangeTableEntry` sets SELECT for a relation read in FROM;
/// the target of a DML statement its command's privilege; row locking
/// UPDATE).
pub(crate) fn note_relation(relid: PgClassOid, perms: u32) {
    with_relation(current_level_is_sublink(), |req| {
        let rel = req.relation(relid);
        rel.perms |= perms;
        // The statement targets the relation with the command whose
        // privilege addRangeTableEntry records. The auxiliary UPDATE of an
        // INSERT ... ON CONFLICT DO UPDATE is noted on the same INSERT
        // target (see [`note_conflict_update`]).
        if perms & bits::INSERT != 0 {
            rel.target = Some(TargetCommand::Insert);
        } else if perms & bits::DELETE != 0 {
            rel.target = Some(TargetCommand::Delete);
        } else if perms & bits::UPDATE != 0 && rel.target != Some(TargetCommand::Insert) {
            rel.target = Some(TargetCommand::Update);
        }
    });
}

/// The statement's INSERT target takes the SELECT path of an `ON CONFLICT
/// DO SELECT` (PostgreSQL 19): PG checks the conflicting row against the
/// SELECT policies, and against the UPDATE ones' USING clauses when the
/// row is locked (the lock's UPDATE privilege is noted on its own).
pub(crate) fn note_conflict_select(relid: PgClassOid, _locks: bool) {
    with_relation(current_level_is_sublink(), |req| {
        req.relation(relid).conflict_select = true;
    });
}

/// The statement's INSERT target takes the auxiliary UPDATE path of an
/// `ON CONFLICT DO UPDATE` (`root->onConflict->action == ONCONFLICT_UPDATE`):
/// PG also applies the UPDATE policies to the conflicting row and the new
/// one. Nothing else makes a relation both INSERT's and UPDATE's target.
pub(crate) fn note_conflict_update(relid: PgClassOid) {
    with_relation(current_level_is_sublink(), |req| {
        req.relation(relid).conflict_update = true;
    });
}

/// The statement's `MERGE` target: PG checks the UPDATE, DELETE and INSERT
/// policies of a MERGE whatever action (if any) a row takes.
pub(crate) fn note_merge_target(relid: PgClassOid) {
    with_relation(current_level_is_sublink(), |req| {
        req.relation(relid).merge_target = true
    });
}

/// The statement reads relation `relid` in FROM (`addRangeTableEntry`): its
/// rows are scanned, which a view expands into its query.
pub(crate) fn note_scan(relid: PgClassOid) {
    with_relation(current_level_is_sublink(), |req| {
        req.relation(relid).scanned = true
    });
}

/// The statement locks relation `relid` (FOR UPDATE / SHARE / ...):
/// `ACL_SELECT_FOR_UPDATE`, which is UPDATE.
pub(crate) fn note_lock(relid: PgClassOid) {
    with_relation(current_level_is_sublink(), |req| {
        let rel = req.relation(relid);
        rel.perms |= bits::UPDATE;
        rel.locked = true;
    });
}

/// Run `f`, the analysis of a query level whose FROM items named in
/// `aliases` the planner certainly proves empty: their scans are dummy
/// relations, whose security quals are never built.
pub(crate) fn with_empty_from_aliases<R>(aliases: &[String], f: impl FnOnce() -> R) -> R {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            EMPTY_FROM_ALIASES.with(|a| {
                a.borrow_mut().pop();
            });
        }
    }
    EMPTY_FROM_ALIASES.with(|a| a.borrow_mut().push(aliases.to_vec()));
    let _restore = Restore;
    f()
}

/// Whether the FROM item named `alias` in the level being analysed is
/// certainly empty (a dummy relation the planner never scans).
pub(crate) fn from_item_is_empty(alias: &str) -> bool {
    EMPTY_FROM_ALIASES.with(|a| {
        a.borrow()
            .last()
            .is_some_and(|frame| frame.iter().any(|a| a == alias))
    })
}

/// The scan of `relid` is a dummy relation: no policy expression of it is
/// initialized.
pub(crate) fn note_proven_empty(relid: PgClassOid) {
    with_relation(current_level_is_sublink(), |req| {
        req.relation(relid).proven_empty = true;
    });
}

/// [`note_routine`] for a routine a parameter DEFAULT the plan expands runs:
/// its OID comes from the catalog's `RoutineDefault`, not from resolving a
/// name again, so a stored expression's `pg_depend` filtering keeps it.
pub(crate) fn note_default_routine(oid: PgProcOid, kind: RoutineKind) {
    with(|req| {
        if !req.routines.iter().any(|(o, _)| *o == oid) {
            req.routines.push((oid, kind));
        }
        if !req.stable_routines.contains(&oid) {
            req.stable_routines.push(oid);
        }
    });
}

/// The statement reads column `name` of relation `relid`
/// (`markVarForSelectPriv`), which also requires SELECT of the relation.
/// `level`: the query level whose parse resolved the reference (see
/// [`with_relation`]).
pub(crate) fn note_column_by_name(relid: PgClassOid, name: &str, sublink: bool) {
    note_column_at(relid, name, ColumnAccess::Select, sublink, false);
}

/// The statement reads column `name` of its own DML target: the read is a
/// `markVarForSelectPriv` on the target's `RTEPermissionInfo`, which gives
/// the statement's target SELECT and, with it, the relation's SELECT
/// policies as WITH CHECK options (`get_row_security_policies`).
pub(crate) fn note_target_column_by_name(relid: PgClassOid, name: &str, sublink: bool) {
    note_column_at(relid, name, ColumnAccess::Select, sublink, true);
}

/// The statement accesses column `name` of relation `relid`: reads it, or
/// writes it from the INSERT target list or an UPDATE SET list.
pub(crate) fn note_column(relid: PgClassOid, name: &str, access: ColumnAccess) {
    note_column_at(relid, name, access, current_level_is_sublink(), false);
}

/// [`note_column`], for a reference the parse of a sublink's level resolved
/// (`sublink`: see [`with_relation`]) — and whether the reference is one of
/// the statement's DML target's own columns (`target`).
fn note_column_at(
    relid: PgClassOid,
    name: &str,
    access: ColumnAccess,
    sublink: bool,
    target: bool,
) {
    with_relation(sublink, |req| {
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
        if target && access == ColumnAccess::Select {
            rel.target_select = true;
        }
    });
}

/// A whole-row reference to relation `relid` (`t`, `t.*` in an
/// expression): every column is read. `level`: see [`with_relation`].
pub(crate) fn note_whole_row(relid: PgClassOid, sublink: bool) {
    note_whole_row_at(relid, sublink, false);
}

/// A whole-row reference to one of the statement's own DML target's rows:
/// `markVarForSelectPriv` on the target (see [`note_target_column_by_name`]).
pub(crate) fn note_target_whole_row(relid: PgClassOid, sublink: bool) {
    note_whole_row_at(relid, sublink, true);
}

fn note_whole_row_at(relid: PgClassOid, sublink: bool, target: bool) {
    with_relation(sublink, |req| {
        let rel = req.relation(relid);
        if !rel.selected.contains(&Column::WholeRow) {
            rel.selected.push(Column::WholeRow);
        }
        rel.perms |= bits::SELECT;
        if target {
            rel.target_select = true;
        }
    });
}

/// The statement calls routine `oid` — as a window function when
/// `windowed` (`ExecInitWindowAgg` names even an aggregate a function).
/// `certainty`: what each argument certainly is (see
/// [`crate::expr::plan_fold::certainly_in`]), for the planner's folding.
/// A call of `nextval` / `currval` / `setval` naming its sequence by a
/// constant also requires the sequence's privileges.
pub(crate) fn note_call(
    snapshot: &PgCatalog,
    oid: PgProcOid,
    windowed: bool,
    site: CallSite,
    args: &[Option<typedpg_pg_query::ast::Node>],
    certainty: &[crate::expr::plan_fold::Certain],
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
    // EXPLAIN without ANALYZE: the planner still inlines an inlinable SQL
    // body, or folds an IMMUTABLE call over constants by running it.
    if let Some(plan) = planned_body(snapshot, proc, oid, windowed, site, args, certainty) {
        with(|req| merge_planned_body(&mut req.planned_bodies, oid, plan));
    }
    // nextval / currval / setval (sequence.c): checked as they run.
    let catalog = snapshot.pg_catalog_oid() == Some(proc.pronamespace);
    let needed = match proc.proname.as_str() {
        "nextval" if catalog => bits::USAGE | bits::UPDATE,
        "currval" if catalog => bits::SELECT | bits::USAGE,
        "setval" if catalog => bits::UPDATE,
        _ => return,
    };
    if let Some(seq) = args
        .first()
        .and_then(|a| constant_regclass(snapshot, a.as_ref()))
    {
        note_sequence(seq, needed);
    }
}

/// What a call of `proc` does to its body when the statement is only
/// planned (`EXPLAIN` without `ANALYZE`), beyond its EXECUTE:
///
/// - `evaluate_function` folds an IMMUTABLE call whose arguments are all
///   constants — a set-returning or `record` result isn't folded, and a
///   STRICT call with a certainly NULL argument returns NULL without
///   running — so its body really runs;
/// - `inline_set_returning_function` replaces a `FROM` scan by an inlinable
///   SQL function's query, whose relations are then planned;
/// - `inline_function` substitutes a single-expression SQL body, whose
///   routines are initialized with the plan. The analyzer reads exactly
///   those bodies (`inline_function`'s own refusals — STRICT, a SET item, a
///   volatility mismatch — only make this keep more checks).
///
/// A call the planner certainly leaves alone contributes only its EXECUTE.
///
/// A STABLE call over constants is folded only while *estimating* a
/// restriction (`estimate_expression_value`: the pseudoconstant side of a
/// `WHERE` / `JOIN ... ON` comparison, a `LIMIT` / `OFFSET` bound, a
/// `TABLESAMPLE` parameter — see [`crate::expr::plan_fold::estimating`]); a
/// target list or a pseudoconstant qual never folds it.
fn planned_body(
    snapshot: &PgCatalog,
    proc: &crate::pg_catalog::PgProc,
    oid: PgProcOid,
    windowed: bool,
    site: CallSite,
    args: &[Option<typedpg_pg_query::ast::Node>],
    certainty: &[crate::expr::plan_fold::Certain],
) -> Option<PlannedBody> {
    use crate::pg_catalog::{ProKind, ProVolatile};
    if windowed || proc.prokind != ProKind::Function {
        return None;
    }
    // `evaluate_function` folds a call while planning — in a target list, a
    // qual, or a `FROM` function scan — when every argument is a constant
    // and the function is IMMUTABLE, or STABLE in estimation mode: the body
    // runs then. A set-returning or `record` result is never folded, and a
    // STRICT call with a certainly NULL argument yields NULL without running.
    if !proc.proretset && proc.prorettype != crate::pg_catalog::oid::RECORD {
        let mut all_const = true;
        let mut null_arg = false;
        for &arg in certainty {
            match arg {
                crate::expr::plan_fold::Certain::Not => all_const = false,
                crate::expr::plan_fold::Certain::Null => null_arg = true,
                _ => {}
            }
        }
        let volatile_folds = proc.provolatile == ProVolatile::Immutable
            || (proc.provolatile == ProVolatile::Stable && crate::expr::plan_fold::estimating());
        if all_const && volatile_folds && !(proc.proisstrict && null_arg) {
            return Some(PlannedBody::Runs);
        }
    }
    match site {
        CallSite::From => {
            // `inline_set_returning_function`: a plain function, SQL
            // language, not STRICT, not VOLATILE, not VOID, not SECURITY
            // DEFINER, no SET items, and arguments with no volatile call or
            // subplan.
            if !proc.proretset
                || proc.prolang != crate::pg_catalog::SQL_LANGUAGE
                || proc.proisstrict
                || proc.provolatile == ProVolatile::Volatile
                || proc.prorettype == crate::pg_catalog::oid::VOID
                || snapshot
                    .routine_bodies
                    .get(&oid)
                    .is_some_and(|b| b.security_definer)
                || snapshot.procs_with_config.contains(&oid)
            {
                return None;
            }
            if args.iter().any(|a| contains_sublink(a.as_ref())) {
                return None;
            }
            Some(PlannedBody::Inlined)
        }
        CallSite::Expression => {
            // `inline_function`: the analyzer holds the body of exactly the
            // functions PG can read as a single expression
            // (`inlinable_body`); a SECURITY DEFINER function or one with
            // SET items is never inlined. A scalar body holding a sublink is
            // not inlined either (only `inline_set_returning_function`
            // accepts those): the call stays an `opfuncid` / `funcid` the
            // executor initializes.
            if proc.prorettype != crate::pg_catalog::oid::RECORD
                && !snapshot
                    .routine_bodies
                    .get(&oid)
                    .is_some_and(|b| b.security_definer)
                && !snapshot.procs_with_config.contains(&oid)
                && snapshot
                    .inline_sql_bodies
                    .get(&oid)
                    .is_some_and(|body| !contains_sublink(body.as_ref()))
            {
                return Some(PlannedBody::Inlined);
            }
            None
        }
    }
}

thread_local! {
    /// The operator being resolved is one its call site records itself (see
    /// [`resolve_operator_quietly`]): [`PgCatalog::find_operator_detailed`]
    /// notes no EXECUTE.
    ///
    /// [`PgCatalog::find_operator_detailed`]: crate::pg_catalog::PgCatalog::find_operator_detailed
    static QUIET_OPERATOR: Cell<u32> = const { Cell::new(0) };
}

/// Run `f` — an operator's resolution — without noting its function's
/// EXECUTE: the call site records what the plan really does with it (a
/// STRICT call over a certainly NULL operand becomes NULL before
/// `ExecInitFunc`).
pub(crate) fn resolve_operator_quietly<R>(f: impl FnOnce() -> R) -> R {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            QUIET_OPERATOR.with(|c| c.set(c.get() - 1));
        }
    }
    QUIET_OPERATOR.with(|c| c.set(c.get() + 1));
    let _restore = Restore;
    f()
}

/// Whether the operator being resolved is one its call site records itself
/// (see [`resolve_operator_quietly`]).
pub(crate) fn quiet_operator() -> bool {
    QUIET_OPERATOR.with(Cell::get) > 0
}

/// Whether a call of `proc` with operands of the given certainty is replaced
/// by NULL before the executor (or the planner) initializes and runs its
/// function: `evaluate_function`'s STRICT-with-a-NULL-input shortcut, which
/// applies to function and operator calls alike (a set-returning or `record`
/// result is never folded, so it is no such call). The arguments are still
/// simplified first — what that runs is kept by the caller.
pub(crate) fn folds_to_null(
    proc: &crate::pg_catalog::PgProc,
    certainty: &[crate::expr::plan_fold::Certain],
) -> bool {
    proc.prokind == ProKind::Function
        && proc.proisstrict
        && !proc.proretset
        && proc.prorettype != crate::pg_catalog::oid::RECORD
        && certainty.contains(&crate::expr::plan_fold::Certain::Null)
}

/// The statement calls operator function `proc` — an `OpExpr`'s
/// `opfuncid`, which `ExecInitFunc` checks and `evaluate_function` may run
/// while planning exactly like a `FuncExpr`'s. The caller has already
/// applied [`folds_to_null`]: a call it replaces by NULL contributes
/// nothing here.
pub(crate) fn note_operator(
    snapshot: &PgCatalog,
    proc: &crate::pg_catalog::PgProc,
    certainty: &[crate::expr::plan_fold::Certain],
) {
    if !active() {
        return;
    }
    note_routine(proc.oid, RoutineKind::Function);
    note_operator_folding(snapshot, proc, certainty);
}

/// The plan-time part of an operator's call, beyond its EXECUTE: what
/// `evaluate_function` runs, or `inline_function` plans, of its body.
pub(crate) fn note_operator_folding(
    snapshot: &PgCatalog,
    proc: &crate::pg_catalog::PgProc,
    certainty: &[crate::expr::plan_fold::Certain],
) {
    if !active() {
        return;
    }
    if let Some(plan) = planned_body(
        snapshot,
        proc,
        proc.oid,
        false,
        CallSite::Expression,
        &[],
        certainty,
    ) {
        with(|req| merge_planned_body(&mut req.planned_bodies, proc.oid, plan));
    }
}

/// [`note_operator_folding`], for an operator-like node the planner only
/// ever *folds* — never inlines: a `NULLIF`, a `ScalarArrayOpExpr`
/// (`eval_const_expressions` only calls `ece_evaluate_expr` on them when
/// every argument is a constant). Only a folded call's body runs.
pub(crate) fn note_operator_folded(
    snapshot: &PgCatalog,
    proc: &crate::pg_catalog::PgProc,
    certainty: &[crate::expr::plan_fold::Certain],
) {
    if !active() {
        return;
    }
    if planned_body(
        snapshot,
        proc,
        proc.oid,
        false,
        CallSite::Expression,
        &[],
        certainty,
    ) == Some(PlannedBody::Runs)
    {
        with(|req| merge_planned_body(&mut req.planned_bodies, proc.oid, PlannedBody::Runs));
    }
}

/// Whether `node` holds a sublink or a subplan at this level
/// (`contain_subplans`): `inline_set_returning_function` refuses such an
/// argument.
fn contains_sublink(node: Option<&typedpg_pg_query::ast::Node>) -> bool {
    use typedpg_pg_query::NodeRef;
    let Some(inner) = node else {
        return false;
    };
    let mut todo = vec![inner.to_ref()];
    while let Some(n) = todo.pop() {
        if matches!(n, NodeRef::SubLink(_) | NodeRef::SubPlan(_)) {
            return true;
        }
        todo.extend(n.children());
    }
    false
}

/// The relation a constant `regclass` argument names (`'s'`,
/// `'s'::regclass`), as `regclassin` resolves it.
fn constant_regclass(
    snapshot: &PgCatalog,
    arg: Option<&typedpg_pg_query::ast::Node>,
) -> Option<PgClassOid> {
    use typedpg_pg_query::ast::{self};
    let text = match arg? {
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
                    Some(typedpg_pg_query::ast::Node::String(s)) => Some(s.sval.as_str()),
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
    with_parse_time(|req| {
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

/// The statement reads relation `relid` with `inh = false` (`FROM ONLY`).
pub(crate) fn note_only(relid: PgClassOid) {
    with_relation(current_level_is_sublink(), |req| {
        req.relation(relid).only = true
    });
}

/// The statement assigns a value of type `ty` (`None`: unknown) to column
/// `name` of `relid`: a domain column's checks run only when the assignment
/// coerces the value (`coerce_type`).
pub(crate) fn note_column_value(relid: PgClassOid, name: &str, ty: Option<PgTypeOid>) {
    with(|req| {
        req.relation(relid)
            .assigned
            .push((Column::Named(name.to_owned()), ty));
    });
}

/// An expression the statement evaluates coerces a value to `ty`, a domain
/// (`CoerceToDomain`): the domain's constraints run.
pub(crate) fn note_domain_coercion(ty: PgTypeOid) {
    with(|req| {
        if !req.domain_coercions.contains(&ty) {
            req.domain_coercions.push(ty);
        }
    });
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

/// The statement inserts a row into partition `leaf` (or into `relid`
/// itself): the planner initializes only the result relations the rows may
/// reach (`ExecFindPartition`, pruned at plan time).
pub(crate) fn note_insert_route(relid: PgClassOid, leaf: PgClassOid) {
    with(|req| {
        let rel = req.relation(relid);
        if !rel.insert_leaves.contains(&leaf) {
            rel.insert_leaves.push(leaf);
        }
    });
}

/// A row of the INSERT into `relid` whose partition the analyzer can't
/// tell: every partition may be reached.
pub(crate) fn note_insert_route_unknown(relid: PgClassOid) {
    with(|req| req.relation(relid).insert_route_unknown = true);
}

/// The statement writes relation `relid` (INSERT / UPDATE / DELETE, the
/// `TRIGGER_TYPE_*` bit), which fires its triggers for that event. `only`:
/// the write reaches no inheritance children (`UPDATE ONLY` / `DELETE FROM
/// ONLY`).
/// A utility statement's own privilege check (see [`UtilityCheck`]).
pub(crate) fn note_utility(check: UtilityCheck) {
    with(|req| req.utility.push(check.clone()));
}

/// `TRUNCATE` empties `relid`: its TRUNCATE statement triggers fire. The
/// relation is in no range table, so no row-level security applies.
pub(crate) fn note_truncated(relid: PgClassOid) {
    with(|req| {
        let rel = req.relation(relid);
        rel.no_range_table = true;
        let write = WriteEvent {
            event: crate::ddl::triggers::TRIGGER_TYPE_TRUNCATE,
            only: true,
        };
        if !rel.writes.contains(&write) {
            rel.writes.push(write);
        }
    });
}

pub(crate) fn note_write(relid: PgClassOid, event: i32, only: bool) {
    with(|req| {
        let write = WriteEvent { event, only };
        let rel = req.relation(relid);
        if !rel.writes.contains(&write) {
            rel.writes.push(write);
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

/// What a trigger body is walked for: the trigger, the relation whose
/// trigger row fires (a partition's clone, for a row trigger defined on a
/// partitioned table), and the event.
#[derive(Clone, Copy)]
struct TriggerFiring<'a> {
    trigger: &'a Trigger,
    relid: PgClassOid,
    /// `TRIGGER_TYPE_*` of the firing.
    event: i32,
}

/// The part of a firing that changes what a body may do, for the
/// once-per-firing check: the trigger row (its relation, event, timing,
/// level and transition tables) and the event.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct FiringKey {
    trigger: crate::oid::PgGenericOid,
    event: i32,
}

/// What makes two body checks different: the routine, the identity it runs
/// as, the textual search path it inherits and the path that resolves to,
/// and the trigger firing it is walked for.
#[derive(Clone, PartialEq, Eq, Hash)]
struct VisitKey {
    routine: PgProcOid,
    role: String,
    setting: Vec<String>,
    path: Vec<PgNamespaceOid>,
    firing: Option<FiringKey>,
    /// How the body is reached: a body reached both planned and only
    /// simplified is checked for each.
    mode: PlannedBody,
}

/// Where a body the check is inside runs: the routine, and how a statement
/// came to run it.
#[derive(Clone, Debug)]
enum FrameKind {
    Function {
        signature: String,
        kind: RoutineKind,
    },
    Trigger {
        signature: String,
        /// `TRIGGER_TYPE_*`.
        event: i32,
        /// The relation written, as `schema.name`.
        relation: String,
    },
}

#[derive(Clone, Debug)]
struct RunFrame {
    kind: FrameKind,
    /// The effective role the body runs as (`GetUserId()`).
    role: String,
    /// The effective role is the routine's owner because it is `SECURITY
    /// DEFINER`.
    definer: bool,
}

thread_local! {
    /// The bodies the current check is inside, outermost first.
    static RUN_FRAMES: RefCell<Vec<RunFrame>> = const { RefCell::new(Vec::new()) };
}

/// Pops the frame [`push_frame`] pushed, however the check ends.
struct FrameGuard;

impl Drop for FrameGuard {
    fn drop(&mut self) {
        RUN_FRAMES.with(|f| {
            f.borrow_mut().pop();
        });
    }
}

fn push_frame(frame: RunFrame) -> FrameGuard {
    RUN_FRAMES.with(|f| f.borrow_mut().push(frame));
    FrameGuard
}

thread_local! {
    /// The transition tables the trigger body being checked declares:
    /// `(name, the relation whose rows they hold)`.
    static TRANSITION_TABLES: RefCell<Vec<(String, PgClassOid)>> =
        const { RefCell::new(Vec::new()) };
}

/// The relation a trigger body's transition table `name` holds the rows of,
/// while that body is being checked (`REFERENCING NEW TABLE AS new_rows`).
pub(crate) fn transition_table(name: &str) -> Option<PgClassOid> {
    TRANSITION_TABLES.with(|tables| {
        tables
            .borrow()
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|&(_, relid)| relid)
    })
}

/// Puts the transition tables in scope that [`transition_table`] reports,
/// and restores the enclosing ones however the check ends. A body without
/// any leaves none in scope: a routine it calls doesn't see them.
struct TransitionGuard {
    outer: Vec<(String, PgClassOid)>,
}

impl TransitionGuard {
    fn enter(tables: Option<(PgClassOid, &[(String, bool)])>) -> Self {
        let next = match tables {
            Some((relid, tables)) => tables
                .iter()
                .map(|(name, _)| (name.clone(), relid))
                .collect(),
            None => Vec::new(),
        };
        let outer =
            TRANSITION_TABLES.with(|current| std::mem::replace(&mut *current.borrow_mut(), next));
        TransitionGuard { outer }
    }
}

impl Drop for TransitionGuard {
    fn drop(&mut self) {
        TRANSITION_TABLES.with(|tables| *tables.borrow_mut() = self.outer.clone());
    }
}

/// The note locating a body failure: which routine ran, why, and as whom.
fn frames_note() -> Option<String> {
    RUN_FRAMES.with(|frames| {
        let frames = frames.borrow();
        if frames.is_empty() {
            return None;
        }
        let mut parts: Vec<String> = Vec::new();
        for (i, frame) in frames.iter().rev().enumerate() {
            let lead = if i == 0 { "in" } else { "called by" };
            match &frame.kind {
                FrameKind::Function { signature, kind } => {
                    parts.push(format!("{lead} {} {signature}", kind.word()));
                }
                FrameKind::Trigger {
                    signature,
                    event,
                    relation,
                } => {
                    parts.push(format!("{lead} trigger function {signature}"));
                    parts.push(format!(
                        "fired by {} on {relation}",
                        crate::ddl::triggers::event_word(*event)
                    ));
                }
            }
        }
        // A routine the statement itself calls (no outer frame): say so.
        if matches!(
            frames.first().map(|f| &f.kind),
            Some(FrameKind::Function { .. })
        ) {
            parts.push("called by the statement".to_owned());
        }
        let inner = frames.last()?;
        parts.push(if inner.definer {
            format!(
                "run as its owner {} (SECURITY DEFINER)",
                quote_role(&inner.role)
            )
        } else {
            format!("run as role {}", quote_role(&inner.role))
        });
        Some(parts.join(", "))
    })
}

impl Denial {
    fn into_error(self) -> AnalyzeError {
        let mut raw = RawError::new(
            AnalyzeError::InsufficientPrivilege(self.message),
            None,
            Some(format!("grant it in a migration: {}", self.grant)),
        );
        if let Some(location) = frames_note() {
            raw.notes.push(location);
        }
        raw.notes.push(self.note);
        raw.finalize_implicit()
    }
}

fn quote_role(name: &str) -> String {
    typedpg_core::quote_identifier(name)
}

/// Whether `sel` is PostgreSQL's simple `UNION ALL` (`is_simple_union_all`):
/// the planner pulls it up into an append relation, so each leaf's unused
/// outputs are dropped. No ORDER BY, LIMIT / OFFSET, locking clause or WITH,
/// and every node of the tree a `UNION ALL` (the leaves' types are checked
/// where the set operation is resolved, see [`note_setop_simple`]).
fn simple_union_all(sel: &typedpg_pg_query::ast::SelectStmt) -> bool {
    use typedpg_pg_query::ast::SetOperation;
    if sel.op == SetOperation::SetopNone
        || !sel.sort_clause.is_empty()
        || sel.limit_offset.is_some()
        || sel.limit_count.is_some()
        || !sel.locking_clause.is_empty()
        || sel.with_clause.is_some()
    {
        return false;
    }
    fn tree(sel: &typedpg_pg_query::ast::SelectStmt) -> bool {
        use typedpg_pg_query::ast::SetOperation;
        if sel.op == SetOperation::SetopNone {
            return true;
        }
        sel.op == SetOperation::SetopUnion
            && sel.all
            && sel.larg.as_deref().is_some_and(tree)
            && sel.rarg.as_deref().is_some_and(tree)
    }
    tree(sel)
}

/// Merge a leaf's kept-output plan into a simple `UNION ALL` tree's group.
fn merge_kept_into(group: usize, kept: &[bool]) {
    LEVEL_OUTPUTS.with(|r| {
        let mut registry = r.borrow_mut();
        let Some(Some(g)) = registry.get_mut(group) else {
            return;
        };
        while g.outputs.len() < kept.len() {
            g.outputs.push(LevelOutput::default());
            g.used.push(false);
        }
        for (output, kept) in g.outputs.iter_mut().zip(kept) {
            output.kept |= *kept;
        }
    });
}

/// Keep every output of a simple `UNION ALL` tree's group.
fn mark_keep_all(group: usize) {
    LEVEL_OUTPUTS.with(|r| {
        if let Some(Some(g)) = r.borrow_mut().get_mut(group) {
            g.keep_all = true;
        }
    });
}

/// The set operation being resolved needs no output coercion (every arm's
/// column already has the tree's type): a simple `UNION ALL` tree keeps its
/// pull-up (`tlist_same_datatypes`). Otherwise every output is evaluated,
/// and the tree's group keeps them all.
pub(crate) fn note_setop_simple(types_match: bool) {
    if types_match {
        return;
    }
    if let Some(Some(group)) = UNION_ALL_GROUPS.with(|groups| groups.borrow().last().copied()) {
        mark_keep_all(group);
    }
}

/// Whether the outputs a view occurrence already expanded (`covered`)
/// include the ones `needed` demands (`None`: every output).
fn covers_outputs(covered: &Option<HashSet<usize>>, needed: &Option<HashSet<usize>>) -> bool {
    match (covered, needed) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(covered), Some(needed)) => needed.is_subset(covered),
    }
}

/// Merge the outputs `needed` into what a view occurrence has expanded.
fn merge_covered_outputs(covered: &mut Option<HashSet<usize>>, needed: &Option<HashSet<usize>>) {
    match (&mut *covered, needed) {
        // Every output is already covered.
        (None, _) => {}
        (_, None) => *covered = None,
        (Some(covered), Some(needed)) => covered.extend(needed.iter().copied()),
    }
}

impl PgCatalog {
    /// The textual `search_path` setting a new session of `role` starts
    /// with: its own `ALTER ROLE ... SET search_path`, the database's, the
    /// every-role one, or the server's default.
    fn role_search_path_setting(&self, role: &str) -> Vec<String> {
        let id = self.roles.id(role);
        self.search_path_guc
            .new_session_of(
                id.and_then(|id| self.roles.search_paths.get(&id))
                    .map(Vec::as_slice),
                self.roles.all_search_path.as_deref(),
            )
            .to_vec()
    }

    /// The `row_security` setting a new session of `role` starts with: its
    /// own `ALTER ROLE ... SET row_security`, the database's, the every-role
    /// one, or on (pg_db_role_setting's precedence, as for `search_path`).
    fn role_row_security_setting(&self, role: &str) -> bool {
        let id = self.roles.id(role);
        id.and_then(|id| self.roles.row_security.get(&id))
            .copied()
            .or(self.database_row_security)
            .or(self.roles.all_row_security)
            .unwrap_or(true)
    }

    /// The search path of a new session of `role` (recomputeNamespacePath):
    /// its setting's schemas that exist and that the role has USAGE on,
    /// `$user` being the schema named after it.
    pub(crate) fn role_search_path(&self, role: &str) -> Vec<PgNamespaceOid> {
        let setting = self.role_search_path_setting(role);
        self.search_path_named(role, &setting)
    }

    /// Whether `role` may run a statement requiring `req`: PG's checks in
    /// the order it makes them — explicitly named schemas (parse
    /// analysis), the range table (`ExecCheckPermissions`), the routines
    /// the plan runs (executor start), then the sequences the statement's
    /// sequence functions and column defaults reach (as they run). What
    /// those routines' bodies then do, and the triggers the statement's
    /// writes fire, is checked recursively.
    pub(crate) fn check_requirements(
        &self,
        role: &str,
        req: &Requirements,
    ) -> Result<(), AnalyzeError> {
        let mut ctx = BodyContext {
            search_path: self.role_search_path_setting(role),
            row_security: self.role_row_security_setting(role),
            ..BodyContext::default()
        };
        self.check_requirements_in(role, req, &mut ctx)
    }

    fn check_requirements_in(
        &self,
        role: &str,
        req: &Requirements,
        ctx: &mut BodyContext,
    ) -> Result<(), AnalyzeError> {
        if SIMPLIFIED_ONLY.with(Cell::get) > 0 {
            return self.check_simplified_requirements(role, req, ctx);
        }
        for &ns in &req.schemas {
            self.check_schema_usage(role, ns)?;
        }
        for check in &req.utility {
            self.check_utility(role, check, ctx)?;
        }

        // EXPLAIN without ANALYZE starts the executor and never runs it:
        // the plan's relations are checked and its functions' EXECUTE
        // compiled, but no expression runs (no `nextval`, no trigger, no
        // routine body, no per-row constraint or index expression).
        let runs = req.executes;
        let mut routines = req.routines.clone();
        // The routines only a row's evaluation reaches (a `CHECK`, an index
        // expression or predicate, a stored generation expression, a
        // partition key): EXPLAIN without ANALYZE initializes none of them,
        // so their EXECUTE is checked only when the statement runs.
        let mut row_routines: Vec<(PgProcOid, RoutineKind)> = Vec::new();
        let mut sequences = if runs {
            req.sequences.clone()
        } else {
            Vec::new()
        };
        let mut defaults = req.defaults.clone();
        let mut planned_bodies = req.planned_bodies.clone();
        let relations = self.expand_views(
            req,
            &mut routines,
            &mut sequences,
            &mut defaults,
            &mut planned_bodies,
        );
        for rel in &relations {
            self.check_relation(role, rel)?;
        }

        // Row-level security: the policies that apply to each relation add
        // their USING quals and WITH CHECK clauses to the query, and those
        // are part of the plan the executor starts (see
        // [`Self::check_row_security`]).
        for rel in &relations {
            self.check_row_security(role, rel, req.executes, ctx)?;
        }

        // Domain coercions the statement's expressions make (`::domain`, an
        // assignment goal): each `CoerceToDomain` runs the domain's checks.
        for &ty in &req.domain_coercions {
            self.collect_domain_constraints(ty, &mut routines, &mut sequences);
        }

        // Column defaults and generation expressions: their routines run
        // with the statement's, and their `nextval` / `currval` / `setval`
        // calls need the sequence's privileges (recorded when the default
        // was defined: a bare `'s'::regclass` names no sequence function).
        for &(relid, attnum) in &defaults {
            let Some(attr) = self
                .attributes_of(relid)
                .iter()
                .find(|a| a.attnum == attnum)
                .cloned()
            else {
                continue;
            };
            // A partitioned table stores no rows: the generation expression
            // the executor evaluates is the physical partition's own (a
            // partition may override it), and the routing happens first.
            if attr.attgenerated == Some(crate::pg_catalog::AttGenerated::Stored)
                && self
                    .pg_class
                    .get(&relid)
                    .is_some_and(|c| c.relkind == RelKind::Partitioned)
            {
                let leaves = self.known_insert_leaves(&relations, relid);
                for part in self.relations_written_with(relid, true, false, &leaves) {
                    if part == relid {
                        continue;
                    }
                    let Some(pa) = self
                        .attributes_of(part)
                        .iter()
                        .find(|a| a.attname == attr.attname)
                        .cloned()
                    else {
                        continue;
                    };
                    if pa.attgenerated == Some(crate::pg_catalog::AttGenerated::Stored) {
                        let (source_rel, source_attnum) =
                            self.stored_expression_source(part, pa.attnum);
                        self.collect_stored_expression(
                            crate::ddl::depend::ObjectAddress::column(source_rel, source_attnum),
                            &mut row_routines,
                            &mut sequences,
                            None,
                        );
                    }
                }
                continue;
            }
            // Dependencies were recorded on the column the expression was
            // written for, not on the inheritance child it was copied to.
            // A default the rewriter puts in the target list is planned and
            // folded with the statement (a stored generation expression is
            // computed later, per row).
            let addr_rel = self.stored_expression_source(relid, attnum);
            if attr.attgenerated.is_some() {
                self.collect_stored_expression(
                    crate::ddl::depend::ObjectAddress::column(addr_rel.0, addr_rel.1),
                    &mut row_routines,
                    &mut sequences,
                    None,
                );
            } else {
                self.collect_stored_expression(
                    crate::ddl::depend::ObjectAddress::column(addr_rel.0, addr_rel.1),
                    &mut routines,
                    &mut sequences,
                    Some(&mut planned_bodies),
                );
            }
            // An inherited serial default's nextval belongs to the column it
            // was created on.
            let source = match self.attr_default_exprs.get(&(relid, attnum)) {
                Some(crate::ddl::tables::check_inherit::StoredExpr::Serial(
                    other,
                    other_attnum,
                )) => (*other, *other_attnum),
                _ => addr_rel,
            };
            if runs && let Some(reqs) = self.default_sequence_perms.get(&source) {
                for &(seq, any_of) in reqs {
                    if !sequences.contains(&(seq, any_of)) {
                        sequences.push((seq, any_of));
                    }
                }
            }
            // A column of a domain type without a default of its own takes
            // the domain's default (build_column_default).
            if !self.attr_default_exprs.contains_key(&(relid, attnum)) {
                self.collect_domain_default(
                    attr.atttypid,
                    &mut routines,
                    &mut sequences,
                    Some(&mut planned_bodies),
                );
            }
        }

        // Reading a virtual generated column runs its generation expression
        // (the rewriter expands the Var into it), as the statement's role.
        for rel in &relations {
            for a in self.attributes_of(rel.relid) {
                if a.attnum <= 0 || a.attgenerated != Some(crate::pg_catalog::AttGenerated::Virtual)
                {
                    continue;
                }
                let read = rel.selected.iter().any(|c| match c {
                    Column::WholeRow => true,
                    Column::Named(name) => *name == a.attname,
                });
                if read {
                    self.collect_stored_expression(
                        crate::ddl::depend::ObjectAddress::column(rel.relid, a.attnum),
                        &mut routines,
                        &mut sequences,
                        Some(&mut planned_bodies),
                    );
                }
            }
        }

        // What the writes evaluate per row, as the statement's role: the
        // relation's CHECK constraints, the expression indexes and partial
        // predicates that are updated, and the domain constraints of the
        // columns it stores.
        for rel in &relations {
            let insert = rel.writes.iter().any(|w| w.event == TRIGGER_TYPE_INSERT);
            let update = rel.writes.iter().any(|w| w.event == TRIGGER_TYPE_UPDATE);
            if !insert && !update {
                continue;
            }
            let only = rel
                .writes
                .iter()
                .any(|w| w.event == TRIGGER_TYPE_UPDATE && w.only);
            let changed = self.changed_attnums(rel, insert);
            let set: Vec<String> = rel
                .updated
                .iter()
                .filter_map(|c| match c {
                    Column::WholeRow => None,
                    Column::Named(name) => Some(name.clone()),
                })
                .collect();
            // The columns a BEFORE ROW UPDATE trigger may change: every
            // stored generated column is then recomputed. `generated` is
            // the target relation's own list (the columns whose domain
            // constraints a write stores); the expressions that run are the
            // storing relations' — a partition's, which may override the
            // parent's.
            let generated = if update {
                self.generated_recomputed_on_update(rel.relid, &set)
            } else {
                Vec::new()
            };
            if update {
                for part in self.relations_written_with(rel.relid, false, only, &[]) {
                    if self
                        .pg_class
                        .get(&part)
                        .is_some_and(|c| c.relkind == RelKind::Partitioned)
                    {
                        continue;
                    }
                    for attnum in self.generated_recomputed_on_update(part, &set) {
                        let (source_rel, source_attnum) =
                            self.stored_expression_source(part, attnum);
                        self.collect_stored_expression(
                            crate::ddl::depend::ObjectAddress::column(source_rel, source_attnum),
                            &mut row_routines,
                            &mut sequences,
                            None,
                        );
                    }
                }
            }
            let leaves = self.known_insert_leaves(&relations, rel.relid);
            let stored = self.relations_written_with(rel.relid, insert, only, &leaves);
            // A partitioned table stores no rows: `UPDATE ONLY` on one
            // evaluates no key.
            let no_row = only
                && self
                    .pg_class
                    .get(&rel.relid)
                    .is_some_and(|c| c.relkind == RelKind::Partitioned);
            if !no_row && (insert || update) {
                for &relid in &stored {
                    self.collect_routing_key_expressions(relid, &mut row_routines, &mut sequences);
                }
            }
            for &relid in &stored {
                self.collect_relation_checks(relid, &mut row_routines, &mut sequences);
            }
            if runs {
                self.collect_written_domains(
                    rel,
                    &changed,
                    &generated,
                    &mut routines,
                    &mut sequences,
                );
            } else {
                // Only the values the plan's target list coerces: a stored
                // generation expression computes its own value (and its
                // domain coercion) per row.
                let planned_changed: Vec<i16> = changed
                    .iter()
                    .copied()
                    .filter(|attnum| {
                        self.attributes_of(rel.relid)
                            .iter()
                            .any(|a| a.attnum == *attnum && a.attgenerated.is_none())
                    })
                    .collect();
                self.collect_written_domains(
                    rel,
                    &planned_changed,
                    &[],
                    &mut routines,
                    &mut sequences,
                );
            }
            for &relid in &stored {
                self.collect_index_expressions(relid, &mut row_routines, &mut sequences);
            }
        }

        // A statement that doesn't run initializes only the plan's
        // expressions: the routines a row's evaluation would reach are not
        // checked (see [`Requirements::executes`]).
        let mut checked_routines = routines;
        if runs {
            for (oid, kind) in row_routines {
                if !checked_routines.iter().any(|(o, _)| *o == oid) {
                    checked_routines.push((oid, kind));
                }
            }
        }

        for &(oid, kind) in &checked_routines {
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

        // A statement that never runs reaches no sequence function: the
        // view queries and defaults expanded above added none.
        if !runs {
            sequences.clear();
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

        // The bodies of the routines the statement calls (or whose
        // defaults run), then the triggers each write it makes fires.
        if runs {
            for &(oid, _) in &checked_routines {
                self.check_routine_body(oid, None, role, ctx, PlannedBody::Runs)?;
            }
            // A strict call's dropped argument was still simplified while
            // planning: what the inlined body's folding ran, ran then.
            for &(oid, plan) in &planned_bodies {
                if plan == PlannedBody::Simplified {
                    self.check_routine_body(oid, None, role, ctx, PlannedBody::Simplified)?;
                }
            }
            for rel in &relations {
                if !rel.writes.is_empty() {
                    self.check_write_triggers(rel, role, ctx)?;
                }
            }
        } else {
            // EXPLAIN without ANALYZE plans the statement and starts its
            // executor: a routine's body is reached only when the planner
            // inlines it into the plan or folds the call by running it
            // (`PlannedBody`); any other body contributes only its EXECUTE.
            for &(oid, plan) in &planned_bodies {
                self.check_routine_body(oid, None, role, ctx, plan)?;
            }
        }
        Ok(())
    }

    /// What an expression the planner drops still does: it resolves names
    /// (schema USAGE) and simplifies — inlining SQL bodies and folding
    /// constants — before the result is discarded. Only what that folding
    /// runs is checked: no relation is scanned, no plan expression
    /// initialized, no sequence reached.
    fn check_simplified_requirements(
        &self,
        role: &str,
        req: &Requirements,
        ctx: &mut BodyContext,
    ) -> Result<(), AnalyzeError> {
        for &ns in &req.schemas {
            self.check_schema_usage(role, ns)?;
        }
        for &(oid, plan) in &req.planned_bodies {
            // An inlined body reached from a dropped expression is only
            // simplified too.
            let mode = match plan {
                PlannedBody::Runs => PlannedBody::Runs,
                PlannedBody::Inlined | PlannedBody::Simplified => PlannedBody::Simplified,
            };
            self.check_routine_body(oid, None, role, ctx, mode)?;
        }
        Ok(())
    }

    /// `LookupExplicitNamespace`: the role needs USAGE on the schema a
    /// statement (or a compiled PL/pgSQL body's declarations) names.
    fn check_schema_usage(&self, role: &str, ns: PgNamespaceOid) -> Result<(), AnalyzeError> {
        if self.has_schema_privilege(role, ns, crate::Privilege::Usage) {
            return Ok(());
        }
        let name = self.namespace_name(ns).unwrap_or_default().to_owned();
        Err(Denial {
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
        .into_error())
    }

    /// Add the routines the stored expression at `addr` runs, and the
    /// operators' underlying functions: what its `pg_depend` rows refer to
    /// (`recordDependencyOnExpr` records an operator, not its function).
    /// `pg_depend` keeps the functions of code the planner then drops (a
    /// dead CASE branch, a COALESCE argument after a constant): execution
    /// never evaluates them, so they need no EXECUTE.
    ///
    /// `planned_folds`: the expression is planned with the statement (a
    /// column default the rewriter puts in its target list) — what the
    /// planner folds of it runs while planning, an `EXPLAIN` without
    /// `ANALYZE` too. `None` for an expression the executor evaluates per
    /// row (a `CHECK`, an index expression, a stored generation expression).
    fn collect_stored_expression(
        &self,
        addr: crate::ddl::depend::ObjectAddress,
        routines: &mut Vec<(PgProcOid, RoutineKind)>,
        sequences: &mut Vec<SequencePrivileges>,
        planned_folds: Option<&mut Vec<(PgProcOid, PlannedBody)>>,
    ) {
        let nodes = self.stored_expression_nodes(addr);
        // What the expression certainly runs, resolved to the exact
        // overloads: a `pg_depend` row keeps the functions of dead code too
        // (a CASE branch the planner folds), and one live call of a name
        // must not resurrect another overload's requirement.
        //
        // The stored expression's names resolved at CREATE-time, under the
        // creation `search_path`; resolving them again here (as the running
        // role) could pick another schema's same-named function and make the
        // created one look unused. So the dead-code walk decides by name and
        // call arity only, which both the expression and its `pg_depend` rows
        // keep; a live call of a name keeps every overload with a matching
        // argument count.
        let live = nodes
            .as_ref()
            .and_then(|nodes| self.live_stored_routines(addr, nodes));
        let callees = nodes.as_ref().map(|nodes| {
            let mut all = crate::expr::plan_fold::Callees::default();
            for node in nodes {
                let one = crate::expr::plan_fold::callees(node.as_ref(), self);
                all.live.extend(one.live);
                all.dead.extend(one.dead);
                all.live_arities.extend(one.live_arities);
                all.dead_arities.extend(one.dead_arities);
            }
            all
        });
        let runs = |name: &str| callees.as_ref().is_none_or(|c| c.runs(name));
        for d in self.pg_depend.iter().filter(|d| {
            d.classid == addr.classid
                && d.objid == addr.objid
                && d.objsubid == addr.objsubid
                && d.deptype == crate::pg_catalog::DepType::Normal
        }) {
            if d.refclassid == crate::pg_catalog::PG_PROC_RELID
                && let Some(oid) = PgProcOid::new(d.refobjid.get())
            {
                let is_live = match (&callees, self.pg_proc.get(&oid)) {
                    (Some(c), Some(proc)) => c.runs_routine(&proc.proname, proc.proargtypes.len()),
                    _ => true,
                };
                if !is_live {
                    continue;
                }
                if !routines.iter().any(|(o, _)| *o == oid) {
                    routines.push((oid, RoutineKind::Function));
                }
            } else if d.refclassid == crate::pg_catalog::PG_OPERATOR_RELID
                && let Some(op) = crate::oid::PgOperatorOid::new(d.refobjid.get())
                && let Some((code, name)) = self
                    .pg_operator
                    .get(&op)
                    .and_then(|o| Some((o.oprcode?, o.oprname.as_str())))
            {
                if !runs(name) {
                    continue;
                }
                if !routines.iter().any(|(o, _)| *o == code) {
                    routines.push((code, RoutineKind::Function));
                }
            }
        }
        // Built-in calls are pinned, so no `pg_depend` edge records them:
        // the analysis' live routines add those the expression resolves.
        // A user function of the same name the expression was created with
        // (a search path naming its schema before `pg_catalog`) shadows the
        // built-in, and the analysis' later resolution may pick the wrong
        // one — its dependencies name the function that ran.
        let shadowed: Vec<&str> = self
            .pg_depend
            .iter()
            .filter(|d| {
                d.classid == addr.classid
                    && d.objid == addr.objid
                    && d.objsubid == addr.objsubid
                    && d.deptype == crate::pg_catalog::DepType::Normal
                    && d.refclassid == crate::pg_catalog::PG_PROC_RELID
            })
            .filter_map(|d| PgProcOid::new(d.refobjid.get()))
            .filter(|oid| oid.get() >= crate::ddl::depend::FIRST_UNPINNED_OBJECT_ID)
            .filter_map(|oid| self.pg_proc.get(&oid).map(|p| p.proname.as_str()))
            .collect();
        if let (Some(planned), Some(live)) = (planned_folds, live.as_ref()) {
            // The expression is planned with the statement (a column
            // default the rewriter puts in the target list): what the
            // planner folds of it runs, an EXPLAIN without ANALYZE too.
            for &(oid, plan) in &live.planned_bodies {
                merge_planned_body(planned, oid, plan);
            }
        }
        if let Some(live) = live {
            // A cast to a domain inside the expression evaluates the
            // domain's checks where the expression does (a plan expression's
            // `CoerceToDomain` at executor start, a per-row one in the row).
            for &ty in &live.domain_coercions {
                self.collect_domain_constraints(ty, routines, sequences);
            }
            for (oid, kind) in live.routines {
                if (oid.get() < crate::ddl::depend::FIRST_UNPINNED_OBJECT_ID
                    || live.stable_routines.contains(&oid))
                    && !self
                        .pg_proc
                        .get(&oid)
                        .is_some_and(|p| shadowed.contains(&p.proname.as_str()))
                    && !routines.iter().any(|(o, _)| *o == oid)
                {
                    routines.push((oid, kind));
                }
            }
            // `nextval` / `currval` / `setval` in a CHECK run when the
            // expression does: the sequence's privileges are checked then.
            // A default's sequences were recorded where it was written
            // (with that search path), a shadowed name names no sequence
            // function at all.
            if shadowed.is_empty() && addr.classid == crate::ddl::depend::PG_CONSTRAINT_RELID {
                for (seq, any_of) in live.sequences {
                    if !sequences.contains(&(seq, any_of)) {
                        sequences.push((seq, any_of));
                    }
                }
            }
        } else if addr.classid == PG_CLASS_RELID
            && addr.objsubid > 0
            && let Some(relid) = PgClassOid::new(addr.objid.get())
            && let Some(crate::ddl::tables::check_inherit::StoredExpr::Written(expr)) =
                self.attr_default_exprs.get(&(relid, addr.objsubid))
        {
            let expr = expr.clone();
            self.collect_builtin_calls_in(relid, expr.as_ref().as_ref(), routines);
        }
    }

    /// The type a column's stored default (or generated) expression has, as
    /// written: `coerce_type` left a value already of the column's type
    /// unchanged. `None` when the analyzer can't read it.
    fn stored_expression_type(
        &self,
        relid: PgClassOid,
        attnum: i16,
    ) -> Option<crate::oid::PgTypeOid> {
        use crate::expr::{Ctx, TypeGoal, infer_expr};
        let crate::ddl::tables::check_inherit::StoredExpr::Written(expr) =
            self.attr_default_exprs.get(&(relid, attnum))?
        else {
            return None;
        };
        let class = self.pg_class.get(&relid)?;
        let nspname = self
            .namespace_name(class.relnamespace)
            .unwrap_or("public")
            .to_owned();
        let attrs = self.attributes_of(relid).to_vec();
        let mut scope = crate::scope::Scope::default();
        scope.add_dml_target(
            self,
            &class.relname,
            crate::qualified_name::QualifiedName::new(nspname, class.relname.clone()),
            &attrs,
        );
        let null_ctx = crate::nullability::NullabilityContext::default();
        let mut params = crate::param_collector::ParamCollector::default();
        let expr = crate::ddl::stored_exprs::over_own_row(self, relid, expr.as_ref().as_ref());
        infer_expr(
            expr.as_ref(),
            Ctx::new(&scope, &null_ctx, self),
            &mut params,
            TypeGoal::NONE,
        )
        .ok()
        .map(|t| t.type_oid)
    }

    /// The routines the stored expression at `addr` certainly runs, as the
    /// analyzer resolves them: analyzing it again with the privileges
    /// collector on, over the relation it belongs to (or a domain's
    /// `VALUE`). `None` when the analyzer can't read the expression.
    fn live_stored_routines(
        &self,
        addr: crate::ddl::depend::ObjectAddress,
        nodes: &[Option<typedpg_pg_query::ast::Node>],
    ) -> Option<Requirements> {
        // The relation whose columns the expression reads, or the base type
        // of the domain whose `VALUE` it reads.
        let (relid, domain_base) = if addr.classid == PG_CLASS_RELID {
            let oid = PgClassOid::new(addr.objid.get())?;
            // An index's expressions read its table's row.
            let relid = match self.pg_index.get(&oid) {
                Some(index) => index.indrelid,
                None => oid,
            };
            (Some(relid), None)
        } else if addr.classid == crate::ddl::depend::PG_CONSTRAINT_RELID {
            let oid = crate::oid::PgConstraintOid::new(addr.objid.get())?;
            if let Some(con) = self.pg_constraint.get(&oid) {
                (Some(con.conrelid), None)
            } else {
                let (domain, _) = self.domain_constraints.iter().find(|(_, constraints)| {
                    constraints.iter().any(|c| c.oid.get() == oid.get())
                })?;
                let base = self.pg_type.get(domain)?.typbasetype?;
                (None, Some(base))
            }
        } else if addr.classid == crate::pg_catalog::PG_TYPE_RELID {
            (None, None)
        } else {
            return None;
        };
        self.analyze_over_row(relid, domain_base, nodes)
    }

    /// The requirements of the expressions `nodes` — what they run and read
    /// — analyzed over relation `relid`'s row (or a domain's `VALUE`), with
    /// the privileges collector active and every stored name resolved
    /// afresh. `None` when the analyzer can't read one of them.
    fn analyze_over_row(
        &self,
        relid: Option<PgClassOid>,
        domain_base: Option<PgTypeOid>,
        nodes: &[Option<typedpg_pg_query::ast::Node>],
    ) -> Option<Requirements> {
        use crate::expr::{Ctx, TypeGoal, infer_expr};
        let (analyzed, required) = suspended(|| {
            collect(|| {
                let null_ctx = crate::nullability::NullabilityContext::default();
                let mut params = crate::param_collector::ParamCollector::default();
                let mut scope = crate::scope::Scope::default();
                if let Some(relid) = relid {
                    let class = self.pg_class.get(&relid)?;
                    let nspname = self
                        .namespace_name(class.relnamespace)
                        .unwrap_or("public")
                        .to_owned();
                    let attrs = self.attributes_of(relid).to_vec();
                    scope.add_dml_target(
                        self,
                        &class.relname,
                        crate::qualified_name::QualifiedName::new(nspname, class.relname.clone()),
                        &attrs,
                    );
                } else if let Some(base) = domain_base {
                    let value = crate::pg_catalog::PgAttribute {
                        attrelid: PG_CLASS_RELID,
                        attname: "value".to_owned(),
                        atttypid: base,
                        attnum: 1,
                        attnotnull: false,
                        atthasdef: false,
                        attgenerated: None,
                        atttypmod: None,
                        attidentity: None,
                        attcollation: None,
                        attislocal: true,
                        attinhcount: 0,
                    };
                    scope.add_dml_target(
                        self,
                        "",
                        crate::qualified_name::QualifiedName::new("", ""),
                        std::slice::from_ref(&value),
                    );
                }
                for node in nodes {
                    let node = match relid {
                        Some(relid) => {
                            crate::ddl::stored_exprs::over_own_row(self, relid, node.as_ref())
                        }
                        None => node.clone(),
                    };
                    let ctx = Ctx::new(&scope, &null_ctx, self);
                    let _ = infer_expr(node.as_ref(), ctx, &mut params, TypeGoal::NONE).ok()?;
                }
                Some(())
            })
        });
        analyzed?;
        Some(required)
    }

    /// The expression a stored object's `pg_depend` rows were recorded for:
    /// a column default or generation expression, a CHECK constraint's
    /// expression, a domain's DEFAULT or CHECK, or an index's expressions
    /// and predicate. `None` when the catalog keeps no expression for it —
    /// then every dependency may run.
    fn stored_expression_nodes(
        &self,
        addr: crate::ddl::depend::ObjectAddress,
    ) -> Option<Vec<Option<typedpg_pg_query::ast::Node>>> {
        use crate::ddl::tables::check_inherit::StoredExpr;
        if addr.classid == PG_CLASS_RELID {
            if addr.objsubid > 0 {
                let relid = PgClassOid::new(addr.objid.get())?;
                return match self.attr_default_exprs.get(&(relid, addr.objsubid)) {
                    Some(StoredExpr::Written(expr)) => Some(vec![(**expr).clone()]),
                    _ => None,
                };
            }
            let index = PgClassOid::new(addr.objid.get())?;
            // A partitioned table's address carries its partition key
            // expressions (an index's its expressions and predicate).
            if let Some(exprs) = self.partition_key_exprs.get(&index) {
                let nodes: Vec<_> = exprs.iter().map(|ast| ast.ast.clone()).collect();
                return (!nodes.is_empty()).then_some(nodes);
            }
            let row = self.pg_index.get(&index)?;
            let nodes: Vec<_> = row
                .indexprs
                .iter()
                .chain(row.indpred.iter())
                .map(|ast| ast.ast.clone())
                .collect();
            return (!nodes.is_empty()).then_some(nodes);
        }
        if addr.classid == crate::ddl::depend::PG_CONSTRAINT_RELID {
            let oid = crate::oid::PgConstraintOid::new(addr.objid.get())?;
            if let Some(def) = self.check_defs.get(&oid) {
                return match &def.expr {
                    StoredExpr::Written(expr) => Some(vec![(**expr).clone()]),
                    _ => None,
                };
            }
            // A domain's named constraint isn't in `check_defs`.
            for c in self.domain_constraints.values().flatten() {
                if c.oid.get() != addr.objid.get() {
                    continue;
                }
                return match &c.expr {
                    Some(StoredExpr::Written(expr)) => Some(vec![(**expr).clone()]),
                    _ => None,
                };
            }
            return None;
        }
        if addr.classid == crate::pg_catalog::PG_TYPE_RELID {
            let ty = PgTypeOid::new(addr.objid.get())?;
            return match self.domain_default_exprs.get(&ty) {
                Some(StoredExpr::Written(expr)) => Some(vec![(**expr).clone()]),
                _ => None,
            };
        }
        None
    }

    /// Add the functions a domain's DEFAULT expression runs, and its
    /// sequence functions, when the domain (the column's own type) has one:
    /// `build_column_default` falls back to it for a column without a
    /// default of its own.
    fn collect_domain_default(
        &self,
        ty: crate::oid::PgTypeOid,
        routines: &mut Vec<(PgProcOid, RoutineKind)>,
        sequences: &mut Vec<SequencePrivileges>,
        planned_folds: Option<&mut Vec<(PgProcOid, PlannedBody)>>,
    ) {
        if !self.domain_defaults.contains(&ty) {
            return;
        }
        self.collect_stored_expression(
            crate::ddl::depend::ObjectAddress::type_(ty),
            routines,
            sequences,
            planned_folds,
        );
        if let Some(perms) = self.domain_default_sequence_perms.get(&ty) {
            for &(seq, any_of) in perms {
                if !sequences.contains(&(seq, any_of)) {
                    sequences.push((seq, any_of));
                }
            }
        }
    }

    /// Add the functions the CHECK constraints of `ty` and of the domains
    /// it is over run: `CoerceToDomain` evaluates each level's constraints
    /// as the value is stored.
    fn collect_domain_constraints(
        &self,
        ty: crate::oid::PgTypeOid,
        routines: &mut Vec<(PgProcOid, RoutineKind)>,
        sequences: &mut Vec<SequencePrivileges>,
    ) {
        let mut current = Some(ty);
        while let Some(t) = current {
            let Some(typ) = self.pg_type.get(&t) else {
                return;
            };
            if typ.typtype != crate::pg_catalog::TypType::Domain {
                return;
            }
            for c in self.domain_constraints.get(&t).into_iter().flatten() {
                self.collect_stored_expression(
                    crate::ddl::depend::ObjectAddress {
                        classid: crate::ddl::depend::PG_CONSTRAINT_RELID,
                        objid: c.oid,
                        objsubid: 0,
                    },
                    routines,
                    sequences,
                    None,
                );
            }
            current = typ.typbasetype;
        }
    }

    /// The column whose stored expression the expression of `relid.attnum`
    /// was copied from: the highest inheritance ancestor whose stored
    /// expression is equal. PG copies a child's default / generation
    /// expression verbatim but records its dependencies only on the column
    /// it was written for.
    fn stored_expression_source(&self, relid: PgClassOid, attnum: i16) -> (PgClassOid, i16) {
        let Some(expr) = self.attr_default_exprs.get(&(relid, attnum)) else {
            return (relid, attnum);
        };
        let Some(name) = self
            .attributes_of(relid)
            .iter()
            .find(|a| a.attnum == attnum)
            .map(|a| a.attname.clone())
        else {
            return (relid, attnum);
        };
        let mut source = (relid, attnum);
        let mut current = relid;
        while let Some(parent) = self
            .pg_inherits
            .iter()
            .find(|i| i.inhrelid == current)
            .map(|i| i.inhparent)
        {
            let Some(pa) = self.attribute_by_name(parent, &name) else {
                break;
            };
            if !self
                .attr_default_exprs
                .get(&(parent, pa.attnum))
                .is_some_and(|e| e.same_as_written(expr))
            {
                break;
            }
            source = (parent, pa.attnum);
            current = parent;
        }
        source
    }

    /// The functions a relation's enforced CHECK constraints run
    /// (`ExecConstraints`).
    fn collect_relation_checks(
        &self,
        relid: PgClassOid,
        routines: &mut Vec<(PgProcOid, RoutineKind)>,
        sequences: &mut Vec<SequencePrivileges>,
    ) {
        for con in self.pg_constraint.values().filter(|c| {
            c.conrelid == relid && c.contype == crate::pg_catalog::ConType::Check && c.conenforced
        }) {
            self.collect_stored_expression(
                crate::ddl::depend::ObjectAddress {
                    classid: crate::ddl::depend::PG_CONSTRAINT_RELID,
                    objid: crate::oid::PgGenericOid::from_nonzero(con.oid.into_nonzero()),
                    objsubid: 0,
                },
                routines,
                sequences,
                None,
            );
            // Its built-in calls, which `pg_depend` doesn't record.
            if let Some(crate::ddl::tables::check_inherit::StoredExpr::Written(expr)) =
                self.check_defs.get(&con.oid).map(|d| d.expr.clone())
            {
                self.collect_builtin_calls_in(relid, expr.as_ref().as_ref(), routines);
            }
        }
    }

    /// The columns a write stores: the INSERT target list (or, without one,
    /// its leading columns), the UPDATE SET list, and `SET … = DEFAULT`'s
    /// columns. An INSERT row is complete, so every column of the relation
    /// is one.
    fn changed_attnums(&self, rel: &RelationRequirement, insert: bool) -> Vec<i16> {
        if insert {
            return self
                .attributes_of(rel.relid)
                .iter()
                .filter(|a| a.attnum > 0)
                .map(|a| a.attnum)
                .collect();
        }
        rel.updated
            .iter()
            .filter_map(|c| match c {
                Column::WholeRow => None,
                Column::Named(name) => self.attribute_by_name(rel.relid, name).map(|a| a.attnum),
            })
            .collect()
    }

    /// The relations a write to `relid` may store rows in: itself, and for
    /// a partitioned table the partitions a row is routed to, and for an
    /// UPDATE / DELETE an inheritance parent's children (whose rows it
    /// reaches) — like the triggers the write fires. `only`: the statement
    /// writes no child (`UPDATE ONLY` / a view over `ONLY`).
    fn relations_written_with(
        &self,
        relid: PgClassOid,
        insert: bool,
        only: bool,
        insert_leaves: &[PgClassOid],
    ) -> Vec<PgClassOid> {
        // A known INSERT route: only the leaf partitions the rows reach get
        // a result relation (the planner prunes the rest).
        if insert && !insert_leaves.is_empty() {
            return insert_leaves.to_vec();
        }
        let mut out = Vec::new();
        let mut todo = vec![relid];
        let mut seen = HashSet::new();
        while let Some(id) = todo.pop() {
            if !seen.insert(id) {
                continue;
            }
            out.push(id);
            let partitioned = self
                .pg_class
                .get(&id)
                .is_some_and(|c| c.relkind == RelKind::Partitioned);
            if !only && (partitioned || !insert) {
                todo.extend(
                    self.pg_inherits
                        .iter()
                        .filter(|i| i.inhparent == id)
                        .map(|i| i.inhrelid),
                );
            }
        }
        out
    }

    /// The stored generated columns an UPDATE of `relid` recomputes
    /// (`ExecInitGenerated`): those whose expression reads a column of the
    /// SET list — or every one when a BEFORE ROW UPDATE trigger may change
    /// more columns.
    pub(crate) fn generated_recomputed_on_update(
        &self,
        relid: PgClassOid,
        set_columns: &[String],
    ) -> Vec<i16> {
        let before_row_trigger = self.triggers.get(&relid).is_some_and(|ts| {
            ts.iter().any(|t| {
                t.row
                    && t.timing & TRIGGER_TYPE_BEFORE != 0
                    && t.events & TRIGGER_TYPE_UPDATE != 0
                    && t.state.fires()
            })
        });
        let set: Vec<i16> = set_columns
            .iter()
            .filter_map(|name| self.attribute_by_name(relid, name).map(|a| a.attnum))
            .collect();
        self.attributes_of(relid)
            .iter()
            .filter(|a| a.attgenerated == Some(crate::pg_catalog::AttGenerated::Stored))
            .filter(|a| {
                before_row_trigger
                    || self
                        .generated_refs
                        .get(&(relid, a.attnum))
                        .is_some_and(|refs| refs.iter().any(|r| set.contains(r)))
            })
            .map(|a| a.attnum)
            .collect()
    }

    /// The domain constraints the columns a write stores run: the column
    /// type's own domain chain always (a value of the domain is coerced even
    /// when NULL), then — for a value that may be non-NULL — the domains
    /// nested in its array elements and composite fields (an INSERT's
    /// omitted column with no default is a NULL constant, whose elements and
    /// fields don't exist).
    fn collect_written_domains(
        &self,
        rel: &RelationRequirement,
        changed: &[i16],
        generated: &[i16],
        routines: &mut Vec<(PgProcOid, RoutineKind)>,
        sequences: &mut Vec<SequencePrivileges>,
    ) {
        let mut attnums: Vec<i16> = changed.to_vec();
        attnums.extend(generated);
        let insert = rel.writes.iter().any(|w| w.event == TRIGGER_TYPE_INSERT);
        for a in self.attributes_of(rel.relid) {
            if a.attnum <= 0 || !attnums.contains(&a.attnum) {
                continue;
            }
            // `coerce_type` returns a value already of the column's type
            // unchanged: no CoerceToDomain, so no domain check. A generated
            // column's expression, or a DEFAULT's, was coerced to the
            // column's type when it was written: a value already of the
            // domain type keeps no `CoerceToDomain`.
            let coerces = if generated.contains(&a.attnum) {
                self.stored_expression_type(rel.relid, a.attnum)
                    .is_none_or(|ty| ty != a.atttypid)
            } else {
                let assigned: Vec<Option<PgTypeOid>> = rel
                    .assigned
                    .iter()
                    .filter(|(c, _)| matches!(c, Column::Named(name) if *name == a.attname))
                    .map(|(_, ty)| *ty)
                    .collect();
                if assigned.is_empty() {
                    // The value is the column's DEFAULT, if it has one; an
                    // omitted column without one gets a NULL constant of the
                    // column's type.
                    match self.attr_default_exprs.get(&(rel.relid, a.attnum)) {
                        Some(_) => self
                            .stored_expression_type(rel.relid, a.attnum)
                            .is_none_or(|ty| ty != a.atttypid),
                        None => true,
                    }
                } else {
                    assigned.iter().any(|ty| *ty != Some(a.atttypid))
                }
            };
            if coerces {
                self.collect_domain_constraints(a.atttypid, routines, sequences);
            }
            let may_be_non_null = !insert
                || rel.inserted.iter().any(|c| match c {
                    Column::WholeRow => true,
                    Column::Named(name) => *name == a.attname,
                })
                || a.attgenerated.is_some()
                || self.attr_default_exprs.contains_key(&(rel.relid, a.attnum));
            if coerces && may_be_non_null {
                let mut seen = HashSet::new();
                self.collect_nested_domain_constraints(a.atttypid, routines, sequences, &mut seen);
            }
        }
    }

    /// Add the constraints of the domains `ty` holds inside it: an array's
    /// element type, a composite's fields, recursively — what storing a
    /// non-NULL value of `ty` coerces.
    fn collect_nested_domain_constraints(
        &self,
        ty: crate::oid::PgTypeOid,
        routines: &mut Vec<(PgProcOid, RoutineKind)>,
        sequences: &mut Vec<SequencePrivileges>,
        seen: &mut HashSet<crate::oid::PgTypeOid>,
    ) {
        if !seen.insert(ty) {
            return;
        }
        let base = self.unwrap_domain(ty);
        let Some(typ) = self.pg_type.get(&base) else {
            return;
        };
        if let Some(elem) = typ.typelem {
            self.collect_domain_constraints(elem, routines, sequences);
            self.collect_nested_domain_constraints(elem, routines, sequences, seen);
        }
        if let Some(relid) = typ.typrelid {
            for a in self.attributes_of(relid) {
                if a.attnum <= 0 {
                    continue;
                }
                self.collect_domain_constraints(a.atttypid, routines, sequences);
                self.collect_nested_domain_constraints(a.atttypid, routines, sequences, seen);
            }
        }
    }

    /// The functions the expression indexes and partial-index predicates of
    /// `relid` run for a write. Whether an UPDATE can go HOT depends on the
    /// data (it needs free space on the row's page), so every index — even
    /// one whose key columns the SET list leaves alone — may be updated:
    /// PG 18 evaluates all their expressions and predicates on a non-HOT
    /// update, and a summarizing index (BRIN) may still be summarized.
    fn collect_index_expressions(
        &self,
        relid: PgClassOid,
        routines: &mut Vec<(PgProcOid, RoutineKind)>,
        sequences: &mut Vec<SequencePrivileges>,
    ) {
        for index in self.pg_index.values().filter(|i| i.indrelid == relid) {
            self.collect_stored_expression(
                crate::ddl::depend::ObjectAddress::relation(index.indexrelid),
                routines,
                sequences,
                None,
            );
            for ast in index.indexprs.iter().chain(index.indpred.iter()) {
                self.collect_builtin_calls(relid, ast, routines);
            }
        }
    }

    /// Add the functions the key expressions of the partitioned table
    /// `relid` run: `ExecFindPartition` evaluates them for every row a write
    /// routes. An `EXPLAIN` without `ANALYZE` initializes none of them.
    fn collect_partition_key_expressions(
        &self,
        relid: PgClassOid,
        routines: &mut Vec<(PgProcOid, RoutineKind)>,
        sequences: &mut Vec<SequencePrivileges>,
    ) {
        let Some(exprs) = self.partition_key_exprs.get(&relid).cloned() else {
            return;
        };
        if exprs.is_empty() {
            return;
        }
        self.collect_stored_expression(
            crate::ddl::depend::ObjectAddress::relation(relid),
            routines,
            sequences,
            None,
        );
        for ast in &exprs {
            self.collect_builtin_calls(relid, ast, routines);
        }
    }

    /// Add what routing a row stored in `relid` evaluates: the key
    /// expressions of every partitioned level it descends through — its own
    /// and its ancestors' (`ExecFindPartition`). A partition's constraint,
    /// checked for every row it stores, is derived from those keys and the
    /// bounds: writing a leaf directly evaluates its ancestors' key
    /// expressions too.
    fn collect_routing_key_expressions(
        &self,
        relid: PgClassOid,
        routines: &mut Vec<(PgProcOid, RoutineKind)>,
        sequences: &mut Vec<SequencePrivileges>,
    ) {
        let mut current = Some(relid);
        let mut seen = HashSet::new();
        while let Some(id) = current {
            if !seen.insert(id) {
                break;
            }
            if self
                .pg_class
                .get(&id)
                .is_some_and(|c| c.relkind == RelKind::Partitioned)
            {
                self.collect_partition_key_expressions(id, routines, sequences);
            }
            current = self
                .pg_inherits
                .iter()
                .find(|i| i.inhrelid == id)
                .map(|i| i.inhparent);
        }
    }

    /// Add the built-in functions a stored expression of `relid` runs.
    /// PG records a dependency on an expression's functions unless the
    /// object is pinned (`FirstUnpinnedObjectId`): a built-in's EXECUTE is
    /// checked at runtime but never recorded in `pg_depend`, so the
    /// expression is analyzed again here — keeping only the built-in calls
    /// (a user function's own dependency is recorded, and resolving it
    /// again could pick another overload along the role's search path).
    fn collect_builtin_calls(
        &self,
        relid: PgClassOid,
        ast: &crate::pg_catalog::SerializedAst,
        routines: &mut Vec<(PgProcOid, RoutineKind)>,
    ) {
        self.collect_builtin_calls_in(relid, ast.ast.as_ref(), routines);
    }

    /// [`Self::collect_builtin_calls`] over an expression node.
    fn collect_builtin_calls_in(
        &self,
        relid: PgClassOid,
        node: Option<&typedpg_pg_query::ast::Node>,
        routines: &mut Vec<(PgProcOid, RoutineKind)>,
    ) {
        // The stored expression's names were resolved when it was defined:
        // analyzing it again looks no schema up (PG resolves none at run
        // time), so no schema's USAGE is the statement's to require.
        let (analyzed, req) = crate::privileges::suspended(|| {
            crate::privileges::collect(|| {
                crate::ddl::volatile::infer_over_relation(self, relid, node, None)
            })
        });
        if !matches!(analyzed, Some(Ok(_))) {
            return;
        }
        for &(oid, kind) in &req.routines {
            if oid.get() < crate::ddl::depend::FIRST_UNPINNED_OBJECT_ID
                && !routines.iter().any(|(o, _)| *o == oid)
            {
                routines.push((oid, kind));
            }
        }
    }

    /// The columns of `base` a view's stored query reads: the
    /// `selectedCols` rewriteTargetView leaves on the base relation's
    /// permission info (the view owner must be able to read every column the
    /// view's definition uses, WHERE included).
    fn selected_on_base(&self, view: PgClassOid, base: PgClassOid) -> Vec<Column> {
        self.view_access
            .get(&view)
            .and_then(|a| a.all().relations.into_iter().find(|r| r.relid == base))
            .map(|r| r.selected)
            .unwrap_or_default()
    }

    /// The range table with the views the statement reads expanded, as the
    /// rewriter does (`ApplyRetrieveRule`): a view's own query reads its
    /// relations as the view's owner (`checkAsUser`), or as the statement's
    /// role for a `security_invoker` view — whose nested occurrence inside
    /// another view is the statement's role again, never the enclosing
    /// view's owner — and its routines run as the statement's role. A view
    /// written through (auto-updatable) writes its base relation as its
    /// owner (`rewriteTargetView`).
    #[allow(clippy::too_many_arguments)]
    fn expand_views(
        &self,
        req: &Requirements,
        routines: &mut Vec<(PgProcOid, RoutineKind)>,
        sequences: &mut Vec<SequencePrivileges>,
        defaults: &mut Vec<(PgClassOid, i16)>,
        planned_bodies: &mut Vec<(PgProcOid, PlannedBody)>,
    ) -> Vec<RelationRequirement> {
        let mut out: Vec<RelationRequirement> = Vec::new();
        let mut todo: std::collections::VecDeque<RelationRequirement> =
            req.relations.iter().cloned().collect();
        let mut expanded: Vec<(
            PgClassOid,
            Option<crate::ddl::roles::RoleId>,
            Option<HashSet<usize>>,
        )> = Vec::new();
        // The views a write through a `CASCADED` CHECK OPTION reaches: their
        // WHERE quals are checked on INSERT too.
        let mut cascaded_write: HashMap<PgClassOid, bool> = HashMap::new();
        while let Some(mut rel) = todo.pop_front() {
            let is_view = self
                .pg_class
                .get(&rel.relid)
                .is_some_and(|c| c.relkind == RelKind::View);
            let instead_for = |event: i32| {
                self.triggers
                    .get(&rel.relid)
                    .is_some_and(|ts| ts.iter().any(|t| t.is_instead_row_for(event)))
            };
            // An auto-updatable view written through: its base relation
            // takes the write, with the columns mapped. A view with an
            // INSTEAD OF row trigger for the command keeps the write: the
            // trigger handles it and the base relation is not written at all
            // (rewriteHandler's `instead`).
            let command_writes = rel.perms & (bits::INSERT | bits::UPDATE | bits::DELETE);
            let mut writes = command_writes;
            if instead_for(crate::ddl::triggers::TRIGGER_TYPE_INSERT) {
                writes &= !bits::INSERT;
            }
            if instead_for(crate::ddl::triggers::TRIGGER_TYPE_UPDATE) {
                writes &= !bits::UPDATE;
            }
            if instead_for(crate::ddl::triggers::TRIGGER_TYPE_DELETE) {
                writes &= !bits::DELETE;
            }
            let writes = if rel.locked && rel.updated.is_empty() {
                writes & !bits::UPDATE
            } else {
                writes
            };
            let upd = self
                .view_updatability
                .get(&rel.relid)
                .filter(|u| u.not_updatable.is_none() && u.base.is_some());
            let write_through = writes != 0 && upd.is_some();
            // A write through this view that a CHECK OPTION (its own, or a
            // CASCADED one above it) checks: an INSERT must evaluate the
            // view's WHERE. UPDATE / DELETE find their rows through the
            // query in any case.
            let inherited_cascaded = cascaded_write.get(&rel.relid).copied().unwrap_or(false);
            let check_qual = self.check_option_views.contains(&rel.relid) || inherited_cascaded;
            // The view's query runs when the statement reads the view (in
            // FROM, or locking it); for UPDATE / DELETE it finds the rows
            // even when the write goes to the base relation; for INSERT it
            // runs for an auto-updatable view with a CHECK OPTION (whose
            // WHERE becomes the WITH CHECK OPTION qual). A view with an
            // INSTEAD OF row trigger for UPDATE / DELETE is handed the rows
            // the query finds (ApplyRetrieveRule); an INSERT a trigger takes
            // never runs it.
            let runs_query = rel.scanned
                || rel.locked
                || ((command_writes & (bits::UPDATE | bits::DELETE) != 0)
                    && (upd.is_some()
                        || instead_for(crate::ddl::triggers::TRIGGER_TYPE_UPDATE)
                        || instead_for(crate::ddl::triggers::TRIGGER_TYPE_DELETE)))
                || ((writes & bits::INSERT != 0) && upd.is_some() && check_qual);
            // The output columns this occurrence of the view reads (`None`:
            // every one of them, a whole-row read included), and whether it
            // reads an expression column at all (a write through the view
            // evaluates only its WHERE).
            let used: Option<HashSet<usize>> = if !runs_query {
                Some(HashSet::new())
            } else if rel.selected.iter().any(|c| matches!(c, Column::WholeRow)) {
                None
            } else {
                let attrs = self.attributes_of(rel.relid);
                let mut used = HashSet::new();
                for c in &rel.selected {
                    let Column::Named(name) = c else {
                        continue;
                    };
                    match attrs.iter().position(|a| a.attname == *name) {
                        Some(i) => {
                            used.insert(i);
                        }
                        None => {
                            used.extend(0..attrs.len());
                        }
                    }
                }
                Some(used)
            };
            if is_view {
                // setRuleCheckAsUser: a security_invoker view's query is
                // checked as the role executing the statement (InvalidOid:
                // the current user, whatever encloses the view); any other
                // view's as its owner.
                let inner_as = if self.security_invoker_views.contains(&rel.relid) {
                    None
                } else {
                    Some(self.owner_of(AclObject::Relation(rel.relid)))
                };
                // The view's query: its relations read, its routines run,
                // its sequence functions called. A write through an
                // auto-updatable view evaluates only the view's WHERE (never
                // its select list), unless the statement reads an expression
                // column of the view (RETURNING, a WHERE of its own): then
                // the whole query runs.
                //
                // Every occurrence of the view is a copy the rewriter
                // rewrote (`ApplyRetrieveRule`) and planned on its own: an
                // occurrence that reads a column an earlier one didn't needs
                // that column's output evaluated too. The expansions are
                // merged, so an output is only ever added.
                let needed: Option<HashSet<usize>> = used.clone();
                // A lock is per occurrence: it must reach the base relations
                // of this one even when an earlier occurrence read more.
                let covered = !rel.locked
                    && expanded
                        .iter()
                        .find(|(relid, check_as, _)| {
                            *relid == rel.relid && *check_as == rel.check_as
                        })
                        .is_some_and(|(_, _, covered)| covers_outputs(covered, &needed));
                if runs_query
                    && !covered
                    && let Some(access) = self.view_access.get(&rel.relid)
                {
                    match expanded
                        .iter_mut()
                        .find(|(relid, check_as, _)| {
                            *relid == rel.relid && *check_as == rel.check_as
                        })
                        .map(|(_, _, covered)| covered)
                    {
                        Some(covered) => merge_covered_outputs(covered, &needed),
                        None => expanded.push((rel.relid, rel.check_as, needed.clone())),
                    }
                    let reads_expression = rel.selected.iter().any(|c| match c {
                        Column::WholeRow => true,
                        Column::Named(name) => {
                            let Some(upd) = upd else {
                                return true;
                            };
                            let Some(i) = self
                                .attributes_of(rel.relid)
                                .iter()
                                .position(|a| a.attname == *name)
                            else {
                                return true;
                            };
                            upd.columns.get(i).is_none_or(|r| r.is_err())
                        }
                    });
                    let qual_only = write_through && !reads_expression;
                    // A write through the view evaluates its WHERE (the
                    // part every column's read runs); a read runs the
                    // outputs it names, unless the planner keeps them.
                    let evaluated = if qual_only {
                        access.shared.clone()
                    } else {
                        access.reading(used.as_ref())
                    };
                    for &(oid, kind) in &evaluated.routines {
                        if !routines.iter().any(|(o, _)| *o == oid) {
                            routines.push((oid, kind));
                        }
                    }
                    for &(seq, any_of) in &evaluated.sequences {
                        if !sequences.contains(&(seq, any_of)) {
                            sequences.push((seq, any_of));
                        }
                    }
                    for &(oid, plan) in &evaluated.planned_bodies {
                        merge_planned_body(planned_bodies, oid, plan);
                    }
                    for &ty in &evaluated.domain_coercions {
                        self.collect_domain_constraints(ty, routines, sequences);
                    }
                    let base = upd.and_then(|u| u.base);
                    let mut inner: Vec<RelationRequirement> = Vec::new();
                    // Only the relations the evaluated part of the query
                    // reads: an output the statement doesn't read is
                    // dropped (`remove_unused_subquery_outputs`), and the
                    // relations of its sublinks are not planned at all —
                    // they must not be checked, as a `security_invoker`
                    // query reads them as the statement's role.
                    for read in &evaluated.relations {
                        // A written-through view's base relation is merged
                        // into the new target RTE (rewriteTargetView): its
                        // permissions come from the view's RTE, not from the
                        // view query's own.
                        if write_through && Some(read.relid) == base {
                            continue;
                        }
                        let entry = match inner.iter().position(|r| r.relid == read.relid) {
                            Some(i) => i,
                            None => {
                                inner.push(RelationRequirement {
                                    relid: read.relid,
                                    check_as: inner_as,
                                    perms: bits::SELECT | if rel.locked { bits::UPDATE } else { 0 },
                                    target: None,
                                    conflict_update: false,
                                    conflict_select: false,
                                    merge_target: false,
                                    target_select: false,
                                    view_owner: inner_as.is_some(),
                                    proven_empty: rel.proven_empty,
                                    no_range_table: false,
                                    selected: Vec::new(),
                                    inserted: Vec::new(),
                                    updated: Vec::new(),
                                    locked: rel.locked,
                                    scanned: true,
                                    writes: Vec::new(),
                                    only: read.only,
                                    assigned: Vec::new(),
                                    insert_leaves: Vec::new(),
                                    insert_route_unknown: false,
                                });
                                inner.len() - 1
                            }
                        };
                        for col in &read.selected {
                            if !inner[entry].selected.contains(col) {
                                inner[entry].selected.push(col.clone());
                            }
                        }
                    }
                    todo.extend(inner);
                }
                if write_through {
                    let base = upd.and_then(|u| u.base).expect("checked above");
                    let view_attrs = self.attributes_of(rel.relid).to_vec();
                    let base_attrs = self.attributes_of(base).to_vec();
                    // The view's base RTE keeps its inheritance flag
                    // (rewriteTargetView): a view over `ONLY parent` writes
                    // no child rows.
                    let base_only = self
                        .view_access
                        .get(&rel.relid)
                        .and_then(|a| a.all().relations.into_iter().find(|r| r.relid == base))
                        .is_some_and(|r| r.only);
                    // The writes rewriteTargetView hands to the base
                    // relation: its triggers fire there, not on the view.
                    let mapped_writes: Vec<WriteEvent> = rel
                        .writes
                        .iter()
                        .copied()
                        .filter(|w| match w.event {
                            TRIGGER_TYPE_INSERT => writes & bits::INSERT != 0,
                            TRIGGER_TYPE_UPDATE => writes & bits::UPDATE != 0,
                            TRIGGER_TYPE_DELETE => writes & bits::DELETE != 0,
                            _ => false,
                        })
                        .map(|mut w| {
                            w.only |= base_only;
                            w
                        })
                        .collect();
                    rel.writes.retain(|w| !mapped_writes.contains(w));
                    // A view column's base column, by name.
                    let base_column = |name: &str| -> Option<i16> {
                        let a = view_attrs.iter().find(|a| a.attname == name)?;
                        let base_num = usize::try_from(a.attnum - 1)
                            .ok()
                            .and_then(|i| upd.expect("checked above").columns.get(i))
                            .and_then(|r| r.as_ref().ok().copied())?;
                        base_attrs
                            .iter()
                            .find(|b| b.attnum == base_num)
                            .map(|b| b.attnum)
                    };
                    let map = |cols: &[Column]| -> Vec<Column> {
                        cols.iter()
                            .filter_map(|c| match c {
                                Column::WholeRow => Some(Column::WholeRow),
                                Column::Named(name) => {
                                    let attnum = base_column(name)?;
                                    base_attrs
                                        .iter()
                                        .find(|b| b.attnum == attnum)
                                        .map(|b| Column::Named(b.attname.clone()))
                                }
                            })
                            .collect()
                    };
                    // rewriteTargetListIU fills a column's default with the
                    // *view's* default when the view has one; a column the
                    // view leaves to nothing keeps its place in the base
                    // target list only for an INSERT, where the base's own
                    // default then applies (a DELETE / UPDATE of the view
                    // sets DEFAULT to NULL instead).
                    let mut assigned: Vec<i16> = rel
                        .inserted
                        .iter()
                        .filter_map(|c| match c {
                            Column::WholeRow => None,
                            Column::Named(name) => base_column(name),
                        })
                        .collect();
                    // An explicit `DEFAULT` in a view column without a
                    // default of its own doesn't assign the base column:
                    // rewriteTargetListIU removes it so the base's default
                    // applies on the recursive rewrite.
                    for &(relid, attnum) in defaults.iter() {
                        if relid != rel.relid
                            || self.attr_default_exprs.contains_key(&(relid, attnum))
                        {
                            continue;
                        }
                        if let Some(base_attnum) = view_attrs
                            .iter()
                            .find(|a| a.attnum == attnum)
                            .map(|a| a.attname.as_str())
                            .and_then(base_column)
                        {
                            assigned.retain(|a| *a != base_attnum);
                        }
                    }
                    let mut kept_defaults = Vec::new();
                    for &(relid, attnum) in defaults.iter() {
                        if relid != rel.relid {
                            continue;
                        }
                        if self.attr_default_exprs.contains_key(&(relid, attnum)) {
                            // The view's own default runs.
                            kept_defaults.push((relid, attnum));
                            if let Some(base_attnum) = view_attrs
                                .iter()
                                .find(|a| a.attnum == attnum)
                                .map(|a| a.attname.as_str())
                                .and_then(base_column)
                            {
                                assigned.push(base_attnum);
                            }
                        }
                    }
                    defaults.retain(|&(relid, attnum)| {
                        relid != rel.relid || kept_defaults.contains(&(relid, attnum))
                    });
                    if writes & bits::INSERT != 0 {
                        for a in base_attrs.iter().filter(|a| a.attnum > 0) {
                            if !assigned.contains(&a.attnum)
                                && !defaults.contains(&(base, a.attnum))
                            {
                                defaults.push((base, a.attnum));
                            }
                        }
                    }
                    if writes & bits::INSERT != 0
                        && check_qual
                        && (self.cascaded_check_option_views.contains(&rel.relid)
                            || inherited_cascaded)
                    {
                        *cascaded_write.entry(base).or_insert(false) = true;
                    }
                    todo.push_back(RelationRequirement {
                        relid: base,
                        check_as: inner_as,
                        perms: (rel.perms & !(bits::INSERT | bits::UPDATE | bits::DELETE)) | writes,
                        // rewriteTargetView points the query's result
                        // relation at the base table: the statement's
                        // command, and its auxiliary ON CONFLICT UPDATE,
                        // apply to the base's policies.
                        target: if writes & bits::INSERT != 0 {
                            Some(TargetCommand::Insert)
                        } else if writes & bits::DELETE != 0 {
                            Some(TargetCommand::Delete)
                        } else if writes & bits::UPDATE != 0 {
                            Some(TargetCommand::Update)
                        } else {
                            None
                        },
                        conflict_update: rel.conflict_update,
                        conflict_select: rel.conflict_select,
                        merge_target: false,
                        // rewriteTargetView gives the base RTE the view
                        // RTE's requiredPerms: the SELECT an outer RETURNING
                        // or ON CONFLICT inference needed stays needed.
                        target_select: rel.target_select,
                        view_owner: inner_as.is_some(),
                        proven_empty: false,
                        no_range_table: false,
                        // rewriteTargetView leaves the view query's own
                        // selectedCols on the base relation, not the outer
                        // statement's: the owner needs SELECT on every
                        // column its query reads whenever the base relation
                        // must be read at all (the view RTE's requiredPerms).
                        selected: self.selected_on_base(rel.relid, base),
                        inserted: map(&rel.inserted),
                        updated: map(&rel.updated),
                        locked: false,
                        scanned: false,
                        writes: mapped_writes,
                        only: base_only,
                        insert_leaves: rel.insert_leaves.clone(),
                        insert_route_unknown: rel.insert_route_unknown,
                        assigned: rel
                            .assigned
                            .iter()
                            .filter_map(|(c, ty)| match c {
                                Column::WholeRow => Some((Column::WholeRow, *ty)),
                                Column::Named(name) => {
                                    let attnum = base_column(name)?;
                                    base_attrs
                                        .iter()
                                        .find(|b| b.attnum == attnum)
                                        .map(|b| (Column::Named(b.attname.clone()), *ty))
                                }
                            })
                            .collect(),
                    });
                }
            }
            match out
                .iter_mut()
                .find(|r| r.relid == rel.relid && r.check_as == rel.check_as)
            {
                Some(existing) => {
                    existing.perms |= rel.perms;
                    existing.only |= rel.only;
                    existing.conflict_update |= rel.conflict_update;
                    existing.conflict_select |= rel.conflict_select;
                    existing.merge_target |= rel.merge_target;
                    existing.target_select |= rel.target_select;
                    existing.view_owner |= rel.view_owner;
                    existing.no_range_table &= rel.no_range_table;
                    existing.proven_empty = if rel.target.is_some() || !rel.writes.is_empty() {
                        false
                    } else if !rel.scanned {
                        existing.proven_empty
                    } else if existing.target.is_some() || !existing.writes.is_empty() {
                        false
                    } else if existing.scanned {
                        existing.proven_empty && rel.proven_empty
                    } else {
                        rel.proven_empty
                    };
                    if existing.target.is_none() {
                        existing.target = rel.target;
                    }
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
                    for assignment in &rel.assigned {
                        if !existing.assigned.contains(assignment) {
                            existing.assigned.push(assignment.clone());
                        }
                    }
                    for write in &rel.writes {
                        if !existing.writes.contains(write) {
                            existing.writes.push(*write);
                        }
                    }
                    for leaf in &rel.insert_leaves {
                        if !existing.insert_leaves.contains(leaf) {
                            existing.insert_leaves.push(*leaf);
                        }
                    }
                    existing.insert_route_unknown |= rel.insert_route_unknown;
                }
                None => out.push(rel),
            }
        }
        out
    }

    /// The search path a body that sets `search_path` to `names` sees:
    /// `$user` is `role`, schemas that don't exist or that `role` has no
    /// USAGE on are skipped (recomputeNamespacePath), and duplicates keep
    /// their first place.
    fn search_path_named(&self, role: &str, names: &[String]) -> Vec<PgNamespaceOid> {
        let mut out = Vec::new();
        for name in names {
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

    /// The display form of routine `proc`: `schema.name(arg, ...)`.
    fn routine_signature(&self, proc: &crate::pg_catalog::PgProc) -> String {
        let schema = self.namespace_name(proc.pronamespace).unwrap_or_default();
        let args = proc
            .proargtypes
            .iter()
            .map(|&t| crate::ddl::util::format_type_for_message(self, t))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{}({args})",
            crate::QualifiedName::new(schema, proc.proname.clone())
        )
    }

    /// The display form of relation `relid`.
    fn relation_display_name(&self, relid: PgClassOid) -> String {
        let Some(class) = self.pg_class.get(&relid) else {
            return "<unknown relation>".to_owned();
        };
        let schema = self.namespace_name(class.relnamespace).unwrap_or_default();
        crate::QualifiedName::new(schema, class.relname.clone()).to_string()
    }

    /// Check the body of routine `oid` as the role the caller runs it as
    /// (`firing`: the trigger that called it, and the event). A routine
    /// with no recorded body — a built-in one, or a language the analyzer
    /// doesn't read — passes.
    ///
    /// `mode`: how the body is reached (see [`PlannedBody`]): run, planned
    /// by an `EXPLAIN` without `ANALYZE` — its relations and routines are
    /// checked, no expression is evaluated — or only simplified.
    fn check_routine_body(
        &self,
        oid: PgProcOid,
        firing: Option<TriggerFiring<'_>>,
        caller_role: &str,
        ctx: &mut BodyContext,
        mode: PlannedBody,
    ) -> Result<(), AnalyzeError> {
        let planned = mode != PlannedBody::Runs;
        let Some(body) = self.routine_bodies.get(&oid) else {
            return Ok(());
        };
        let Some(proc) = self.pg_proc.get(&oid) else {
            return Ok(());
        };
        let effective = if body.security_definer {
            self.roles
                .name(self.owner_of(AclObject::Function(oid)))
                .to_owned()
        } else {
            caller_role.to_owned()
        };
        // The search path the body resolves its names along: its own `SET
        // search_path`, else the one in effect at the call site. A
        // `SECURITY DEFINER` body does not read the owner's `ALTER ROLE ...
        // SET search_path` (`fmgr_security_definer` changes the identity,
        // not the GUC); `$user` and the USAGE filtering are applied to the
        // current identity below.
        let setting = match &body.search_path {
            Some(names) => names.clone(),
            None => ctx.search_path.clone(),
        };
        let path = self.search_path_named(&effective, &setting);
        // A body runs once per firing: a cycle of trigger functions, a
        // function that calls itself, or one trigger function shared by
        // several triggers, ends here.
        let visited = ctx.visited.insert(VisitKey {
            routine: oid,
            role: effective.clone(),
            setting: setting.clone(),
            path: path.clone(),
            firing: firing.map(|f| FiringKey {
                trigger: f.trigger.oid,
                event: f.event,
            }),
            mode,
        });
        if !visited {
            return Ok(());
        }
        let signature = self.routine_signature(proc);
        let kind = match firing {
            Some(f) => FrameKind::Trigger {
                signature,
                event: f.event,
                relation: self.relation_display_name(f.relid),
            },
            None => FrameKind::Function {
                signature,
                kind: match proc.prokind {
                    ProKind::Procedure => RoutineKind::Procedure,
                    ProKind::Aggregate => RoutineKind::Aggregate,
                    _ => RoutineKind::Function,
                },
            },
        };
        let _frame = push_frame(RunFrame {
            kind,
            role: effective.clone(),
            definer: body.security_definer,
        });
        // The trigger's transition tables are in scope in its function's
        // own statements, holding the trigger relation's rows; a body
        // without any leaves none in scope for the routines it calls.
        let _transitions =
            TransitionGuard::enter(firing.map(|f| (f.relid, &f.trigger.transition_tables[..])));
        let mut owned;
        let catalog: &PgCatalog = if path == self.search_path {
            self
        } else {
            owned = self.clone();
            owned.search_path = path;
            &owned
        };
        let params = crate::ddl::function_body::input_params(proc);
        let saved_path = std::mem::replace(&mut ctx.search_path, setting);
        // A body's statements run later, as their own statements: the
        // rewriter of each starts with an empty `activeRIRs`, so a policy
        // whose qual calls a routine that reads its table is not a rewrite
        // loop (`SECURITY DEFINER` is the usual way to keep it from being an
        // execution loop either).
        let saved_rls = std::mem::take(&mut ctx.rls_active);
        // A body the planner only inlined is analyzed as a plan: no
        // expression of it is evaluated, so only what the executor
        // initializes is checked. A body a *simplified* expression runs is
        // checked whole: the flag the simplified check set has to go.
        let _planned = planned.then(PlannedBodyGuard::enter);
        let _simplified = SimplifiedGuard::set(mode == PlannedBody::Simplified);
        let result = match (body.language.as_str(), &body.source) {
            ("sql", RoutineSource::SqlText(text)) => {
                self.check_sql_text_body(catalog, text, proc, &params, &effective, ctx)
            }
            // PG stores an SQL-standard body analyzed: its objects are
            // fixed at CREATE FUNCTION, and running it looks no name up.
            ("sql", RoutineSource::SqlStandard(required)) => {
                if planned {
                    let mut required = required.clone();
                    required.executes = false;
                    self.check_requirements_in(&effective, &required, ctx)
                } else {
                    self.check_requirements_in(&effective, required, ctx)
                }
            }
            ("plpgsql", RoutineSource::Plpgsql { create_sql }) => self.check_plpgsql_body(
                catalog,
                oid,
                create_sql,
                proc,
                firing.as_ref(),
                &params,
                &effective,
                ctx,
            ),
            // A language the analyzer doesn't read: its body isn't SQL.
            _ => Ok(()),
        };
        ctx.search_path = saved_path;
        ctx.rls_active = saved_rls;
        result
    }

    /// Check every statement of a `LANGUAGE sql` body given as a string.
    fn check_sql_text_body(
        &self,
        catalog: &PgCatalog,
        text: &str,
        proc: &crate::pg_catalog::PgProc,
        params: &[(String, crate::oid::PgTypeOid)],
        role: &str,
        ctx: &mut BodyContext,
    ) -> Result<(), AnalyzeError> {
        let Ok(parsed) = typedpg_pg_query::parse(text) else {
            return Ok(());
        };
        for raw in &parsed.tree.stmts {
            let Some(statement) = raw.stmt.as_ref() else {
                continue;
            };
            self.check_body_statement(catalog, statement, proc, params, &[], role, ctx)?;
        }
        Ok(())
    }

    /// Check one body statement: analyze it for what it requires, and check
    /// that. A statement that doesn't analyze is skipped — only its
    /// privileges would have been checked.
    #[allow(clippy::too_many_arguments)]
    fn check_body_statement(
        &self,
        catalog: &PgCatalog,
        statement: &typedpg_pg_query::ast::Node,
        proc: &crate::pg_catalog::PgProc,
        params: &[(String, crate::oid::PgTypeOid)],
        locals: &[(String, ExprType)],
        role: &str,
        ctx: &mut BodyContext,
    ) -> Result<(), AnalyzeError> {
        if !is_analyzable(statement) {
            return Ok(());
        }
        let param_types: Vec<crate::oid::PgTypeOid> = params.iter().map(|(_, t)| *t).collect();
        let namespace = crate::expr::SqlFunctionParams {
            name: proc.proname.clone(),
            params: params.to_vec(),
            locals: locals.to_vec(),
        };
        let (result, requirements) = collect(|| {
            crate::expr::with_sql_function_params(namespace, || {
                crate::resolve::without_execution(|| {
                    crate::resolve::analyze_raw_node_with_param_types(
                        catalog,
                        statement,
                        &param_types,
                    )
                })
            })
        });
        if result.is_err() {
            return Ok(());
        }
        self.check_requirements_in(role, &requirements, ctx)
    }

    /// Check a `LANGUAGE plpgsql` body: every SQL query its statements
    /// carry, with the body's variables in scope, skipping what the
    /// statement provably never runs (see this module's policy). Dynamic
    /// SQL is skipped; the expressions it is given with `USING` are not.
    #[allow(clippy::too_many_arguments)]
    fn check_plpgsql_body(
        &self,
        catalog: &PgCatalog,
        oid: PgProcOid,
        create_sql: &str,
        proc: &crate::pg_catalog::PgProc,
        firing: Option<&TriggerFiring<'_>>,
        params: &[(String, crate::oid::PgTypeOid)],
        role: &str,
        ctx: &mut BodyContext,
    ) -> Result<(), AnalyzeError> {
        let parsed = match ctx.plpgsql.get(&oid) {
            Some(parsed) => parsed.clone(),
            None => {
                // The compiler resolves the body's declared types and
                // `%TYPE` / `%ROWTYPE` names now, at the first call: the
                // schemas they name need USAGE as the role running it,
                // whatever a dead block's statements would have done. The
                // signature's own types were resolved at CREATE FUNCTION
                // and need none (the compiler's body phase reports only the
                // declarations' lookups; see `record_compiled_schemas`).
                let (parsed, schemas) =
                    crate::pg_catalog::pg_query_catalog::record_compiled_schemas(|| {
                        typedpg_pg_query::parse_plpgsql_with_catalog(create_sql, catalog)
                    });
                let parsed = parsed.ok().map(std::sync::Arc::new);
                ctx.plpgsql.insert(oid, parsed.clone());
                ctx.plpgsql_schemas.insert(oid, schemas);
                parsed
            }
        };
        let Some(parsed) = parsed else {
            return Ok(());
        };
        for &ns in ctx.plpgsql_schemas.get(&oid).into_iter().flatten() {
            self.check_schema_usage(role, ns)?;
        }
        let Some(function) = parsed.first() else {
            return Ok(());
        };
        let tg = firing.map(|f| TgValues::of(f, catalog));
        let mut tg = tg;
        if let Some(tg) = tg.as_mut() {
            tg.ignore_assigned(function);
        }
        let locals = self.plpgsql_locals(catalog, function, firing.map(|f| f.relid));
        // `FOR r IN query` and `SELECT ... INTO r` give `r` the query's
        // result shape. A record assigned on several paths has several: a
        // statement reading it is checked for each shape it may have.
        let mut shapes: ShapeEnv = ShapeEnv::new();
        let mut queries: Vec<(String, ShapeEnv)> = Vec::new();
        if let Some(action) = function.action.as_ref() {
            let walk = PlWalk {
                tg: tg.as_ref(),
                datums: &function.datums,
                catalog,
            };
            walk_plpgsql_block_statement(action, walk, &mut shapes, true, &mut queries);
        }
        for (query, env) in queries {
            let Some(statement) = statement_or_expression(&query) else {
                continue;
            };
            let (variants, truncated) =
                self.record_shape_variants(catalog, &env, proc, params, &locals);
            for locals in variants {
                self.check_body_statement(catalog, &statement, proc, params, &locals, role, ctx)?;
            }
            if truncated {
                // More shape combinations than the analyzer tries: the
                // statement is checked for the ones it tried, and every
                // routine its calls could resolve to is required by name and
                // argument count — the union over-resolves, so no denial a
                // combination could reach is missed.
                let mut required = Requirements {
                    executes: !PLANNED_STATEMENT.with(Cell::get)
                        && PLANNED_BODY.with(Cell::get) == 0,
                    ..Requirements::default()
                };
                self.candidate_overloads(catalog, &statement, &mut required.routines);
                self.check_requirements_in(role, &required, ctx)?;
            }
        }
        Ok(())
    }

    /// The variables a PL/pgSQL body's statements see: its parameters and
    /// `DECLARE`d variables, `NEW` / `OLD` (the trigger relation's row
    /// type), `FOUND` and `TG_*`.
    fn plpgsql_locals(
        &self,
        catalog: &PgCatalog,
        function: &PlFunction,
        trigger_rel: Option<PgClassOid>,
    ) -> Vec<(String, ExprType)> {
        let mut locals: Vec<(String, ExprType)> = Vec::new();
        if let Some(relid) = trigger_rel
            && let Some(row_type) = self.table_row_type(relid)
        {
            let mut row = ExprType::scalar(row_type, true);
            // `NEW` / `OLD` are the relation's row: `NEW.x` resolves.
            row.record_fields = self.composite_fields(row_type);
            locals.push(("new".to_owned(), row.clone()));
            locals.push(("old".to_owned(), row));
        }
        for datum in &function.datums {
            let Some(name) = datum.refname() else {
                continue;
            };
            if trigger_rel.is_some() && (name == "new" || name == "old") {
                // Already typed as the trigger relation's row.
                continue;
            }
            let value = match datum {
                PlDatum::Var(var) | PlDatum::Promise(var) => {
                    let Some(datatype) = var.datatype.as_ref() else {
                        continue;
                    };
                    let Some(type_oid) = resolve_type_name(catalog, datatype) else {
                        continue;
                    };
                    // `r t;` / `r t%ROWTYPE`: the composite type's fields.
                    let mut value = ExprType::scalar(type_oid, true);
                    value.record_fields = self.composite_fields(type_oid);
                    value
                }
                // A record: its shape is unknown unless a query gave it
                // ([`Self::add_record_shapes`]).
                PlDatum::Rec(_) | PlDatum::Row(_) => {
                    ExprType::scalar(crate::pg_catalog::oid::RECORD, true)
                }
                PlDatum::Recfield(_) => continue,
            };
            locals.push((name.to_owned(), value));
        }
        locals
    }

    /// The fields of a composite type, as a record's shape (`r t;`,
    /// `r t%ROWTYPE`), so `r.x` analyzes.
    fn composite_fields(&self, ty: crate::oid::PgTypeOid) -> Option<crate::expr::RecordShape> {
        let relid = self.pg_type.get(&ty)?.typrelid?;
        let fields: Vec<crate::expr::RecordField> = self
            .attributes_of(relid)
            .iter()
            .filter(|a| a.attnum > 0)
            .map(|a| crate::expr::RecordField {
                name: a.attname.clone(),
                ty: ExprType::scalar_with_typmod(a.atttypid, !a.attnotnull, a.atttypmod),
            })
            .collect();
        (!fields.is_empty()).then_some(crate::expr::RecordShape {
            fields,
            hidden: false,
        })
    }

    /// The composite type of relation `relid`'s row (`pg_type.typrelid`).
    fn table_row_type(&self, relid: PgClassOid) -> Option<crate::oid::PgTypeOid> {
        self.pg_type
            .values()
            .find(|t| {
                t.typrelid == Some(relid) && t.typtype == crate::pg_catalog::TypType::Composite
            })
            .map(|t| t.oid)
    }

    /// The locals each shape of the record variables in `env` makes: a
    /// record takes the fields of the query whose result it holds
    /// (`SELECT … INTO`, `FOR … IN`), or of the record it was assigned
    /// from. A record with several possible shapes yields one set of locals
    /// per shape combination, so a statement is analyzed for each.
    fn record_shape_variants(
        &self,
        catalog: &PgCatalog,
        env: &ShapeEnv,
        proc: &crate::pg_catalog::PgProc,
        params: &[(String, crate::oid::PgTypeOid)],
        locals: &[(String, ExprType)],
    ) -> (Vec<Vec<(String, ExprType)>>, bool) {
        let param_types: Vec<crate::oid::PgTypeOid> = params.iter().map(|(_, t)| *t).collect();
        // The shape of a query's columns, as the record's fields.
        let query_shape = |query: &str| -> Option<Vec<crate::expr::RecordField>> {
            let statement = full_statement(query)?;
            if !is_analyzable(&statement) {
                return None;
            }
            let namespace = crate::expr::SqlFunctionParams {
                name: proc.proname.clone(),
                params: params.to_vec(),
                locals: locals.to_vec(),
            };
            let analyzed = crate::expr::with_sql_function_params(namespace, || {
                crate::resolve::without_execution(|| {
                    crate::resolve::analyze_raw_node_with_param_types(
                        catalog,
                        &statement,
                        &param_types,
                    )
                })
            });
            let Ok((columns, _)) = analyzed else {
                return None;
            };
            (!columns.is_empty()).then(|| {
                columns
                    .iter()
                    .map(|c| crate::expr::RecordField {
                        name: c.name.clone(),
                        ty: ExprType::scalar_with_typmod(c.type_oid, c.nullable, c.typmod),
                    })
                    .collect()
            })
        };
        // A record's shapes, following `r := s`.
        let shapes_of = |name: &str| -> Vec<Vec<crate::expr::RecordField>> {
            let signature = |fields: &[crate::expr::RecordField]| -> Vec<String> {
                fields
                    .iter()
                    .map(|f| {
                        format!(
                            "{}:{:?}:{}:{:?}",
                            f.name, f.ty.type_oid, f.ty.nullable, f.ty.typmod
                        )
                    })
                    .collect()
            };
            let mut out: Vec<Vec<crate::expr::RecordField>> = Vec::new();
            let mut seen_shapes: Vec<Vec<String>> = Vec::new();
            let mut todo: Vec<(String, Vec<ShapeSource>)> =
                vec![(name.to_owned(), env.get(name).cloned().unwrap_or_default())];
            let mut seen = std::collections::HashSet::new();
            while let Some((name, sources)) = todo.pop() {
                if !seen.insert(name.clone()) {
                    continue;
                }
                for source in sources {
                    match source {
                        ShapeSource::Query(query) => {
                            if let Some(fields) = query_shape(&query) {
                                let sig = signature(&fields);
                                if !seen_shapes.contains(&sig) {
                                    seen_shapes.push(sig);
                                    out.push(fields);
                                }
                            }
                        }
                        ShapeSource::Record(other) => {
                            let sources = env.get(&other).cloned().unwrap_or_default();
                            if sources.is_empty() {
                                // NEW / OLD, or a declared composite
                                // variable: the locals carry its fields.
                                if let Some(fields) = locals
                                    .iter()
                                    .find(|(n, _)| *n == other)
                                    .and_then(|(_, ty)| ty.record_fields.clone())
                                {
                                    let fields = fields.fields;
                                    let sig = signature(&fields);
                                    if !seen_shapes.contains(&sig) {
                                        seen_shapes.push(sig);
                                        out.push(fields);
                                    }
                                }
                            } else {
                                todo.push((other.clone(), sources));
                            }
                        }
                    }
                }
            }
            out
        };
        let mut variants: Vec<Vec<(String, ExprType)>> = vec![locals.to_vec()];
        for name in env.keys() {
            let shapes = shapes_of(name);
            if shapes.is_empty() {
                continue;
            }
            let mut next = Vec::with_capacity(variants.len() * shapes.len());
            for variant in &variants {
                for fields in &shapes {
                    let mut variant = variant.clone();
                    let mut value = ExprType::scalar(crate::pg_catalog::oid::RECORD, true);
                    value.record_fields = Some(crate::expr::RecordShape {
                        fields: fields.clone(),
                        hidden: false,
                    });
                    match variant.iter_mut().find(|(n, _)| n == name) {
                        Some((_, ty)) => *ty = value,
                        None => variant.push((name.clone(), value)),
                    }
                    next.push(variant);
                }
            }
            // A pathological number of combinations: the caller checks the
            // ones taken, and requires every candidate overload besides.
            let mut truncated = false;
            if next.len() > 64 {
                next.truncate(64);
                truncated = true;
            }
            variants = next;
            if truncated {
                return (variants, true);
            }
        }
        (variants, false)
    }

    /// Add every routine a call in `statement` could resolve to by name and
    /// argument count: functions (and operators' functions) of the statement,
    /// whether or not the analyzer can tell which overload a record shape
    /// selects. The union over-resolves when several overloads of a name and
    /// arity exist, which keeps every denial a combination could reach.
    fn candidate_overloads(
        &self,
        catalog: &PgCatalog,
        statement: &typedpg_pg_query::ast::Node,
        out: &mut Vec<(PgProcOid, RoutineKind)>,
    ) {
        use typedpg_pg_query::NodeRef;
        let mut note = |oid: PgProcOid, kind: RoutineKind| {
            if !out.iter().any(|(o, _)| *o == oid) {
                out.push((oid, kind));
            }
        };
        let mut todo = vec![statement.to_ref()];
        while let Some(n) = todo.pop() {
            match n {
                NodeRef::FuncCall(fc) => {
                    let names: Vec<&str> = fc
                        .funcname
                        .iter()
                        .filter_map(|n| match n.as_ref()? {
                            typedpg_pg_query::ast::Node::String(s) => Some(s.sval.as_str()),
                            _ => None,
                        })
                        .collect();
                    if let Some((schema, name)) = match names.as_slice() {
                        [name] => Some((None, *name)),
                        [schema, name] => Some((Some(*schema), *name)),
                        _ => None,
                    } {
                        for proc in catalog.find_functions(schema, name) {
                            if call_arity_matches(
                                proc.proargtypes.len(),
                                proc.pronargdefaults,
                                proc.provariadic.is_some(),
                                fc.args.len(),
                            ) {
                                let kind = match proc.prokind {
                                    ProKind::Aggregate => RoutineKind::Aggregate,
                                    ProKind::Procedure => RoutineKind::Procedure,
                                    _ => RoutineKind::Function,
                                };
                                note(proc.oid, kind);
                            }
                        }
                    }
                }
                NodeRef::AExpr(e) => {
                    let names: Vec<&str> = e
                        .name
                        .iter()
                        .filter_map(|n| match n.as_ref()? {
                            typedpg_pg_query::ast::Node::String(s) => Some(s.sval.as_str()),
                            _ => None,
                        })
                        .collect();
                    if let Some(name) = names.last() {
                        let operands = [e.lexpr.is_some(), e.rexpr.is_some()]
                            .into_iter()
                            .filter(|present| *present)
                            .count();
                        for op in catalog.pg_operator.values().filter(|o| o.oprname == *name) {
                            let declared = 1 + usize::from(op.oprleft.is_some());
                            if declared == operands
                                && let Some(code) = op.oprcode
                            {
                                note(code, RoutineKind::Function);
                            }
                        }
                    }
                }
                _ => {}
            }
            todo.extend(n.children());
        }
    }

    /// The partitions the INSERT into `relid` certainly reaches, when the
    /// statement's range table holds the relation.
    fn known_insert_leaves(
        &self,
        relations: &[RelationRequirement],
        relid: PgClassOid,
    ) -> Vec<PgClassOid> {
        relations
            .iter()
            .find(|r| r.relid == relid && r.check_as.is_none())
            .filter(|r| !r.insert_route_unknown)
            .map(|r| r.insert_leaves.clone())
            .unwrap_or_default()
    }

    /// Check the triggers the writes `rel` records fire, one walk per
    /// firing.
    fn check_write_triggers(
        &self,
        rel: &RelationRequirement,
        role: &str,
        ctx: &mut BodyContext,
    ) -> Result<(), AnalyzeError> {
        for write in &rel.writes {
            for (relid, trigger) in self.triggers_fired_by(rel, *write) {
                self.check_fired_trigger(relid, trigger, write.event, role, ctx)?;
            }
            // A row moving to another partition is deleted from its
            // partition and inserted into the destination one
            // (ExecCrossPartitionUpdate): both sides' row triggers fire.
            if write.event == TRIGGER_TYPE_UPDATE && !write.only && self.update_may_move(rel) {
                for (relid, event, trigger) in self.triggers_moved_by(rel) {
                    self.check_fired_trigger(relid, trigger, event, role, ctx)?;
                }
            }
        }
        Ok(())
    }

    /// Check one firing of `trigger`: its `WHEN` condition's routines (they
    /// run per row, as the role running the statement) unless the condition
    /// is a literal that never lets it fire, then its function's body.
    fn check_fired_trigger(
        &self,
        relid: PgClassOid,
        trigger: &Trigger,
        event: i32,
        role: &str,
        ctx: &mut BodyContext,
    ) -> Result<(), AnalyzeError> {
        // PG prepares the WHEN expression whole before evaluating it row by
        // row: every function in it is initialized (its EXECUTE checked),
        // even one a short-circuit later skips. A condition that certainly
        // folds to false / NULL keeps the body from running.
        if !trigger.when_functions.is_empty() {
            let when = Requirements {
                routines: trigger
                    .when_functions
                    .iter()
                    .map(|&oid| (oid, RoutineKind::Function))
                    .collect(),
                ..Requirements::default()
            };
            self.check_requirements_in(role, &when, ctx)?;
        }
        if trigger_when_never_fires(trigger, self) {
            return Ok(());
        }
        self.check_routine_body(
            trigger.function,
            Some(TriggerFiring {
                trigger,
                relid,
                event,
            }),
            role,
            ctx,
            PlannedBody::Runs,
        )
    }

    /// The triggers a write `write` to `rel` fires: the relation's own,
    /// plus the row triggers of the relations its rows may be stored in —
    /// a partitioned table's partitions (a row is routed to one of them),
    /// and for UPDATE / DELETE an inheritance parent's children (whose
    /// rows it reaches). A statement that fails for the rows some partition
    /// takes is checked like one that fails for every row. Disabled
    /// triggers, a row trigger on a partitioned table itself (only its
    /// clones on the partitions run) and `UPDATE OF` triggers whose columns
    /// the SET list doesn't change never fire; `ONLY` reaches no children.
    fn triggers_fired_by(
        &self,
        rel: &RelationRequirement,
        write: WriteEvent,
    ) -> Vec<(PgClassOid, &Trigger)> {
        let mut out = Vec::new();
        let mut todo = vec![rel.relid];
        let mut seen = HashSet::new();
        while let Some(relid) = todo.pop() {
            if !seen.insert(relid) {
                continue;
            }
            let Some(class) = self.pg_class.get(&relid) else {
                continue;
            };
            let is_written = relid == rel.relid;
            let partitioned = class.relkind == RelKind::Partitioned;
            for trigger in self.triggers.get(&relid).into_iter().flatten() {
                // Statement triggers fire on the relation written only.
                if !is_written && !trigger.row {
                    continue;
                }
                // A row trigger on a partitioned table never runs there:
                // the clones on its partitions do.
                if partitioned && trigger.row {
                    continue;
                }
                if !trigger.state.fires() {
                    continue;
                }
                if trigger.events & write.event == 0 {
                    continue;
                }
                if write.event == TRIGGER_TYPE_UPDATE
                    && !trigger.columns.is_empty()
                    && !self.update_of_fires(relid, rel, &trigger.columns)
                {
                    continue;
                }
                out.push((relid, trigger));
            }
            if !write.only && (partitioned || write.event != TRIGGER_TYPE_INSERT) {
                // A known INSERT route initializes only the partitions the
                // rows reach (plan-time pruning): their triggers alone fire.
                let routed: Option<&[PgClassOid]> = (write.event == TRIGGER_TYPE_INSERT
                    && is_written)
                    .then_some(rel.insert_leaves.as_slice())
                    .filter(|leaves| !leaves.is_empty())
                    .filter(|_| !rel.insert_route_unknown);
                match routed {
                    Some(leaves) => {
                        todo.extend(leaves.iter().copied().filter(|leaf| *leaf != relid))
                    }
                    None => todo.extend(
                        self.pg_inherits
                            .iter()
                            .filter(|i| i.inhparent == relid)
                            .map(|i| i.inhrelid),
                    ),
                }
            }
        }
        out
    }

    /// Whether an `UPDATE OF columns` trigger on `relid` fires for an UPDATE
    /// of `rel`: the SET list names one of the columns, or names a column a
    /// stored generated column of the list is computed from (the executor
    /// recomputes it, and PG counts it as updated).
    fn update_of_fires(
        &self,
        relid: PgClassOid,
        rel: &RelationRequirement,
        columns: &[String],
    ) -> bool {
        let updated: Vec<&str> = rel
            .updated
            .iter()
            .filter_map(|c| match c {
                Column::Named(name) => Some(name.as_str()),
                Column::WholeRow => None,
            })
            .collect();
        columns.iter().any(|name| {
            if updated.iter().any(|u| u == name) {
                return true;
            }
            let Some(attr) = self.attribute_by_name(relid, name) else {
                return false;
            };
            if !matches!(
                attr.attgenerated,
                Some(
                    crate::pg_catalog::AttGenerated::Stored
                        | crate::pg_catalog::AttGenerated::Virtual
                )
            ) {
                return false;
            }
            let attrs = self.attributes_of(relid);
            self.generated_refs
                .get(&(relid, attr.attnum))
                .is_some_and(|refs| {
                    refs.iter().any(|a| {
                        attrs
                            .iter()
                            .find(|attr| attr.attnum == *a)
                            .is_some_and(|attr| updated.iter().any(|u| *u == attr.attname))
                    })
                })
        })
    }

    /// The attribute numbers of the columns `rel`'s UPDATE writes.
    fn updated_attnums(&self, rel: &RelationRequirement) -> Vec<i16> {
        rel.updated
            .iter()
            .filter_map(|c| match c {
                Column::Named(name) => self.attribute_by_name(rel.relid, name).map(|a| a.attnum),
                Column::WholeRow => None,
            })
            .collect()
    }

    /// Whether an UPDATE of `rel` may move a row into another partition:
    /// the statement's target is a partitioned table (`ONLY` and a direct
    /// update of a partition keep the row, which then breaks the partition
    /// constraint instead), and the SET list assigns one of its partition
    /// key columns (ExecCrossPartitionUpdate) — or a BEFORE UPDATE row
    /// trigger may: the executor re-routes the row after the triggers ran.
    fn update_may_move(&self, rel: &RelationRequirement) -> bool {
        if self
            .pg_class
            .get(&rel.relid)
            .is_none_or(|c| c.relkind != RelKind::Partitioned)
        {
            return false;
        }
        let updated = self.updated_attnums(rel);
        if !updated.is_empty()
            && self
                .partition_key_attrs
                .get(&rel.relid)
                .is_some_and(|key| key.iter().any(|a| updated.contains(a)))
        {
            return true;
        }
        // A BEFORE ROW UPDATE trigger may set a key column the SET list
        // doesn't. The clones on the partitions are the ones that run.
        let mut todo = vec![rel.relid];
        let mut seen = HashSet::new();
        while let Some(relid) = todo.pop() {
            if !seen.insert(relid) {
                continue;
            }
            let Some(class) = self.pg_class.get(&relid) else {
                continue;
            };
            if class.relkind != RelKind::Partitioned {
                for trigger in self.triggers.get(&relid).into_iter().flatten() {
                    if !trigger.row
                        || !trigger.state.fires()
                        || trigger.timing & TRIGGER_TYPE_BEFORE == 0
                        || trigger.events & TRIGGER_TYPE_UPDATE == 0
                    {
                        continue;
                    }
                    if trigger.columns.is_empty()
                        || self.update_of_fires(relid, rel, &trigger.columns)
                    {
                        return true;
                    }
                }
            }
            todo.extend(
                self.pg_inherits
                    .iter()
                    .filter(|i| i.inhparent == relid)
                    .map(|i| i.inhrelid),
            );
        }
        false
    }

    /// The row triggers a moving UPDATE of `rel` may fire: the DELETE row
    /// triggers of the partitions the rows leave, and the INSERT row
    /// triggers of the partitions they enter.
    fn triggers_moved_by(&self, rel: &RelationRequirement) -> Vec<(PgClassOid, i32, &Trigger)> {
        let mut out = Vec::new();
        let mut todo = vec![rel.relid];
        let mut seen = HashSet::new();
        while let Some(relid) = todo.pop() {
            if !seen.insert(relid) {
                continue;
            }
            let Some(class) = self.pg_class.get(&relid) else {
                continue;
            };
            if class.relkind != RelKind::Partitioned {
                for trigger in self.triggers.get(&relid).into_iter().flatten() {
                    if !trigger.row || !trigger.state.fires() {
                        continue;
                    }
                    for event in [TRIGGER_TYPE_INSERT, TRIGGER_TYPE_DELETE] {
                        if trigger.events & event != 0 {
                            out.push((relid, event, trigger));
                        }
                    }
                }
            }
            todo.extend(
                self.pg_inherits
                    .iter()
                    .filter(|i| i.inhparent == relid)
                    .map(|i| i.inhrelid)
                    .filter(|child| self.partition_bounds.contains_key(child)),
            );
        }
        out
    }

    /// A utility statement's own check (see [`UtilityCheck`]), with PG's
    /// error.
    fn check_utility(
        &self,
        role: &str,
        check: &UtilityCheck,
        ctx: &mut BodyContext,
    ) -> Result<(), AnalyzeError> {
        match check {
            UtilityCheck::Relation {
                relid,
                any_of,
                check_as,
            } => {
                let who = match check_as {
                    Some(id) => self.roles.name(*id).to_owned(),
                    None => role.to_owned(),
                };
                let candidates: Vec<crate::Privilege> = [
                    crate::Privilege::Select,
                    crate::Privilege::Insert,
                    crate::Privilege::Update,
                    crate::Privilege::Delete,
                    crate::Privilege::Truncate,
                    crate::Privilege::Maintain,
                ]
                .into_iter()
                .filter(|p| any_of & p.bit() != 0)
                .collect();
                if candidates
                    .iter()
                    .any(|&p| self.has_table_privilege(&who, *relid, p))
                {
                    return Ok(());
                }
                let Some(class) = self.pg_class.get(relid) else {
                    return Ok(());
                };
                let schema = self.namespace_name(class.relnamespace).unwrap_or_default();
                let qualified = crate::QualifiedName::new(schema, class.relname.clone());
                let whom = if check_as.is_some() {
                    format!(
                        "role {} (which the view's query is checked as, being the view's \
                         owner)",
                        quote_role(&who)
                    )
                } else {
                    format!("role {}", quote_role(&who))
                };
                let names: Vec<&str> = candidates.iter().map(|p| p.name()).collect();
                let note = match names.as_slice() {
                    [one] => format!("{whom} has no {one} on {qualified}"),
                    _ => format!("{whom} has none of {} on {qualified}", names.join(", ")),
                };
                let grant = format!(
                    "GRANT {} ON {qualified} TO {};",
                    names.first().copied().unwrap_or("SELECT"),
                    quote_role(&who)
                );
                Err(Denial {
                    message: format!(
                        "permission denied for {} {}",
                        relkind_word(class.relkind),
                        class.relname
                    ),
                    note,
                    grant,
                }
                .into_error())
            }
            UtilityCheck::SequenceOwner { relid } => {
                let Some(id) = self.roles.id(role) else {
                    return Ok(());
                };
                let owner = self.owner_of(AclObject::Relation(*relid));
                if self
                    .roles
                    .has_privs_of_role(self.roles.canonical(id), owner)
                {
                    return Ok(());
                }
                let Some(class) = self.pg_class.get(relid) else {
                    return Ok(());
                };
                let schema = self.namespace_name(class.relnamespace).unwrap_or_default();
                let qualified = crate::QualifiedName::new(schema, class.relname.clone());
                let mut raw = RawError::new(
                    AnalyzeError::InsufficientPrivilege(format!(
                        "must be owner of sequence {}",
                        class.relname
                    )),
                    None,
                    Some(format!(
                        "make the role a member of the sequence's owner in a migration: GRANT \
                         {} TO {};",
                        quote_role(self.roles.name(owner)),
                        quote_role(role)
                    )),
                );
                raw.notes.push(format!(
                    "RESTART IDENTITY resets {qualified}, which only its owner ({}) may do",
                    quote_role(self.roles.name(owner))
                ));
                Err(raw.finalize_implicit())
            }
            UtilityCheck::SetParameter { name } => {
                if self.has_parameter_privilege(role, name, crate::ddl::acl::bits::SET) {
                    return Ok(());
                }
                let mut raw = RawError::new(
                    AnalyzeError::InsufficientPrivilege(format!(
                        "permission denied to set parameter \"{name}\""
                    )),
                    None,
                    Some(format!(
                        "grant it in a migration: GRANT SET ON PARAMETER {name} TO {};",
                        quote_role(role)
                    )),
                );
                raw.notes.push(format!(
                    "only a superuser may set {name} unless granted SET on it; role {} is \
                     neither",
                    quote_role(role)
                ));
                Err(raw.finalize_implicit())
            }
            UtilityCheck::SetRole { name } => {
                let (Some(member), Some(target)) = (self.roles.id(role), self.roles.id(name))
                else {
                    return Ok(());
                };
                if self.roles.can_set_role(member, target) {
                    return Ok(());
                }
                let mut raw = RawError::new(
                    AnalyzeError::InsufficientPrivilege(format!(
                        "permission denied to set role \"{name}\""
                    )),
                    None,
                    Some(format!(
                        "grant it in a migration: GRANT {} TO {};",
                        quote_role(name),
                        quote_role(role)
                    )),
                );
                raw.notes.push(format!(
                    "role {} is not a member of {} with the SET option",
                    quote_role(role),
                    quote_role(name)
                ));
                Err(raw.finalize_implicit())
            }
            UtilityCheck::SessionAuthorization { name } => {
                let Some(id) = self.roles.id(role) else {
                    return Ok(());
                };
                if self.roles.is_superuser(id) || name.as_str() == role {
                    return Ok(());
                }
                let mut raw = RawError::new(
                    AnalyzeError::InsufficientPrivilege(format!(
                        "permission denied to set session authorization \"{name}\""
                    )),
                    None,
                    None,
                );
                raw.notes.push(format!(
                    "only a superuser session may change its session user; role {} is not one",
                    quote_role(role)
                ));
                Err(raw.finalize_implicit())
            }
            UtilityCheck::AsOwnerOf { relid, req } => {
                let owner = self.owner_of(AclObject::Relation(*relid));
                let owner = self.roles.name(owner).to_owned();
                self.check_requirements_in(&owner, req, ctx)
            }
        }
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
        let word = relkind_word(class.relkind);
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
        let whom = if rel.view_owner {
            format!(
                "role {} (which the view's query reads as, being the view's owner)",
                quote_role(who)
            )
        } else {
            format!("role {}", quote_role(who))
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

    /// Whether row-level security applies to relation `relid` for role
    /// `who` (`check_enable_rls`): the table enabled it, `who` is not a
    /// BYPASSRLS role (a superuser always is), and is not the table's owner
    /// unless `FORCE ROW LEVEL SECURITY` says otherwise. A session's
    /// `row_security` off makes PG raise where this returns true
    /// ([`Self::check_row_security`]).
    fn row_security_applies(&self, relid: PgClassOid, who: RoleId) -> bool {
        if !self.row_security.contains(&relid) {
            return false;
        }
        if self
            .roles
            .get(who)
            .is_some_and(|r| r.superuser || r.bypassrls)
        {
            return false;
        }
        let owner = self.owner_of(AclObject::Relation(relid));
        // object_ownercheck: a role holding the owner's privileges is the
        // owner here.
        !(self.roles.has_privs_of_role(who, owner) && !self.force_row_security.contains(&relid))
    }

    /// The policies of `relid` that apply to command `cmd` for `who`
    /// (`get_policies_for_relation`): the command's own or `ALL` policies,
    /// addressed to PUBLIC or to a role `who` has the privileges of, split
    /// into permissive and restrictive (restrictive ones ordered by name,
    /// the order their WITH CHECK options are checked in).
    fn policies_for(
        &self,
        relid: PgClassOid,
        cmd: char,
        who: RoleId,
    ) -> (Vec<Policy>, Vec<Policy>) {
        let mut permissive = Vec::new();
        let mut restrictive = Vec::new();
        for policy in self.policies.get(&relid).into_iter().flatten() {
            if policy.cmd != '*' && policy.cmd != cmd {
                continue;
            }
            let applies = policy.roles.iter().any(|role| match role {
                PolicyRole::Public => true,
                PolicyRole::Role(id) => self.roles.has_privs_of_role(who, *id),
            });
            if !applies {
                continue;
            }
            if policy.permissive {
                permissive.push(policy.clone());
            } else {
                restrictive.push(policy.clone());
            }
        }
        restrictive.sort_by(|a, b| a.name.cmp(&b.name));
        (permissive, restrictive)
    }

    /// The USING and WITH CHECK expressions a policy was created with. PG
    /// stores them analyzed (by OID); the analyzer keeps the parse trees,
    /// rewritten with the objects they name.
    fn policy_qual(&self, relid: PgClassOid, policy: &Policy) -> (Option<Node>, Option<Node>) {
        self.column_deps
            .policy_quals
            .get(&(relid, policy.name.clone()))
            .map(|(using, with_check)| {
                (
                    using.as_ref().and_then(|c| c.expr.clone()),
                    with_check.as_ref().and_then(|c| c.expr.clone()),
                )
            })
            .unwrap_or_default()
    }

    /// The expressions PG builds from a command's policies
    /// (`add_security_quals` / `add_with_check_options`): the permissive
    /// clauses OR-ed, the restrictive ones AND-ed (security quals) or each
    /// checked on its own (WITH CHECK options). `as_wco` builds the clauses
    /// as WithCheckOptions, `force_using` uses the USING clause even when a
    /// WITH CHECK exists (an `INSERT ... ON CONFLICT DO UPDATE` and a
    /// `MERGE` check the existing row with it).
    fn policy_expressions(
        &self,
        relid: PgClassOid,
        cmd: char,
        who: RoleId,
        as_wco: bool,
        force_using: bool,
    ) -> PolicyClauses {
        let (permissive, restrictive) = self.policies_for(relid, cmd, who);
        let clause = |policy: &Policy| -> Option<Node> {
            let (using, with_check) = self.policy_qual(relid, policy);
            if as_wco && !force_using {
                with_check.or(using)
            } else {
                using
            }
        };
        // `pg_policy`'s `hassublinks`: the policy's USING *or* WITH CHECK
        // holds a subquery (whatever this command uses), which the rewriter
        // takes as the reason to recurse into the new quals.
        let sublinks = |policy: &Policy| {
            let (using, with_check) = self.policy_qual(relid, policy);
            crate::expr::has_sublink(using.as_ref())
                || crate::expr::has_sublink(with_check.as_ref())
        };
        // Each expression PG builds is analysed on its own: the policies
        // whose clauses it combines are its `pg_depend` witnesses, so a
        // routine only another clause (or another command) of the same
        // policy calls is not kept alive by it.
        let mut roots: Vec<(Node, Vec<crate::oid::PgGenericOid>)> = Vec::new();
        let mut oids = Vec::new();
        let mut has_sublinks = false;
        let permissive_clauses: Vec<(Node, crate::oid::PgGenericOid)> = permissive
            .iter()
            .filter_map(|policy| clause(policy).map(|c| (c, policy.oid)))
            .collect();
        // No permissive clause (a policy without one grants nothing): the
        // default-deny `false` leaves every restrictive policy unbuilt.
        if !permissive_clauses.is_empty() {
            for policy in &permissive {
                if clause(policy).is_some() {
                    oids.push(policy.oid);
                    has_sublinks |= sublinks(policy);
                }
            }
            for policy in &restrictive {
                if clause(policy).is_some() {
                    oids.push(policy.oid);
                    has_sublinks |= sublinks(policy);
                }
            }
            let permissive_oids: Vec<crate::oid::PgGenericOid> =
                permissive_clauses.iter().map(|(_, oid)| *oid).collect();
            if as_wco {
                let exprs: Vec<Node> = permissive_clauses.into_iter().map(|(c, _)| c).collect();
                roots.push((or_expr(exprs), permissive_oids));
                for policy in &restrictive {
                    if let Some(qual) = clause(policy) {
                        roots.push((qual, vec![policy.oid]));
                    }
                }
            } else {
                // Security quals: AND(restrictive..., OR(permissive...)).
                let mut args: Vec<Node> = restrictive.iter().filter_map(&clause).collect();
                args.push(or_expr(
                    permissive_clauses.into_iter().map(|(c, _)| c).collect(),
                ));
                roots.push((and_expr(args), oids.clone()));
            }
        }
        PolicyClauses {
            roots,
            oids,
            has_sublinks,
        }
    }

    /// Check the row-security policies that apply to `rel` for the role the
    /// statement runs as. The policy expressions become part of the query:
    /// their functions are initialized with the plan (`ExecInitFunc`, as the
    /// invoking role), and the relations their sublinks read are in the
    /// range table (`ExecCheckPermissions`, as `checkAsUser` — the view's
    /// owner when a view reads the table). `EXPLAIN` without `ANALYZE`
    /// starts the executor, so it checks them too; what the planner folds
    /// away is pruned like any stored expression's dead code.
    fn check_row_security(
        &self,
        role: &str,
        rel: &RelationRequirement,
        executes: bool,
        ctx: &mut BodyContext,
    ) -> Result<(), AnalyzeError> {
        if rel.no_range_table {
            return Ok(());
        }
        let Some(class) = self.pg_class.get(&rel.relid) else {
            return Ok(());
        };
        if !matches!(class.relkind, RelKind::Table | RelKind::Partitioned) {
            return Ok(());
        }
        let who = match rel.check_as {
            Some(id) => self.roles.canonical(id),
            None => match self.roles.id(role).map(|id| self.roles.canonical(id)) {
                Some(id) => id,
                None => return Ok(()),
            },
        };
        if !self.row_security_applies(rel.relid, who) {
            return Ok(());
        }
        // `check_enable_rls`: the role may set `row_security` off to be
        // refused rather than filtered. It raises wherever row security
        // applies, before any policy is looked at, with a hint when the role
        // is the table's owner (whose FORCE made row security apply to it).
        if !ctx.row_security {
            let owner = self
                .roles
                .has_privs_of_role(who, self.owner_of(AclObject::Relation(rel.relid)));
            return Err(crate::pgmsg::row_security_off(&class.relname, owner).finalize_implicit());
        }
        // A policy whose subquery reaches the same relation again would
        // recurse forever. PG rejects the query (`infinite recursion
        // detected in policy`) when the clauses it is adding hold a
        // subquery; a re-application without one still adds the same
        // (relation-free) expressions, so the analyzer may keep skipping it.
        // `activeRIRs` names relations only: a second checkAsUser doesn't
        // change the answer.
        let recursive = ctx.rls_active.contains(&rel.relid);
        let target_select = rel.target_select;
        let mut roots: Vec<(Node, Vec<crate::oid::PgGenericOid>)> = Vec::new();
        let mut policies: Vec<crate::oid::PgGenericOid> = Vec::new();
        let mut has_sublinks = false;
        let mut add = |clauses: PolicyClauses| {
            roots.extend(clauses.roots);
            has_sublinks |= clauses.has_sublinks;
            for oid in clauses.oids {
                if !policies.contains(&oid) {
                    policies.push(oid);
                }
            }
        };
        if rel.merge_target {
            // A MERGE target applies the SELECT policies (as security quals
            // and as checks) when the statement reads it, then the UPDATE
            // policies twice (the existing row through the forced USING
            // check, then the new row through WITH CHECK), the DELETE
            // policies on the existing row, and the INSERT policies on the
            // new one (get_row_security_policies' CMD_MERGE branch).
            if target_select {
                add(self.policy_expressions(rel.relid, 'r', who, false, false));
            }
            add(self.policy_expressions(rel.relid, 'w', who, true, true));
            add(self.policy_expressions(rel.relid, 'w', who, true, false));
            if target_select {
                add(self.policy_expressions(rel.relid, 'r', who, true, true));
            }
            add(self.policy_expressions(rel.relid, 'd', who, true, true));
            add(self.policy_expressions(rel.relid, 'a', who, true, false));
            if target_select {
                add(self.policy_expressions(rel.relid, 'r', who, true, true));
            }
        } else {
            match rel.target {
                // A read: the relation's SELECT policies filter the rows.
                // `SELECT ... FOR UPDATE / SHARE` also requires UPDATE, and
                // PG checks the UPDATE policies first.
                None => {
                    if rel.locked {
                        add(self.policy_expressions(rel.relid, 'w', who, false, false));
                    }
                    add(self.policy_expressions(rel.relid, 'r', who, false, false));
                }
                // INSERT: the WITH CHECK clauses of its policies. A FROM
                // occurrence of the same relation (`INSERT INTO t SELECT …
                // FROM t`) is an RTE of its own whose SELECT policies are
                // security quals; the target's SELECT policies become WITH
                // CHECK options only when the statement reads the target
                // (RETURNING, an ON CONFLICT arbiter). The auxiliary UPDATE
                // of ON CONFLICT DO UPDATE adds the UPDATE policies.
                Some(TargetCommand::Insert) => {
                    if rel.scanned {
                        if rel.locked {
                            add(self.policy_expressions(rel.relid, 'w', who, false, false));
                        }
                        add(self.policy_expressions(rel.relid, 'r', who, false, false));
                    }
                    add(self.policy_expressions(rel.relid, 'a', who, true, false));
                    if target_select {
                        add(self.policy_expressions(rel.relid, 'r', who, true, true));
                    }
                    // ON CONFLICT DO SELECT: the UPDATE policies' USING
                    // clauses (when it locks the row) and the SELECT ones
                    // check the conflicting row (rowsecurity.c).
                    if rel.conflict_select {
                        if rel.perms & bits::UPDATE != 0 {
                            add(self.policy_expressions(rel.relid, 'w', who, true, true));
                        }
                        if target_select {
                            add(self.policy_expressions(rel.relid, 'r', who, true, true));
                        }
                    }
                    if rel.conflict_update {
                        add(self.policy_expressions(rel.relid, 'w', who, true, true));
                        if target_select {
                            add(self.policy_expressions(rel.relid, 'r', who, true, true));
                        }
                        add(self.policy_expressions(rel.relid, 'w', who, true, false));
                        if target_select {
                            add(self.policy_expressions(rel.relid, 'r', who, true, true));
                        }
                    }
                }
                // UPDATE: the USING clauses filter the rows it finds (and
                // the SELECT ones when it reads the table, or when a FROM
                // occurrence of it does), the WITH CHECK clauses verify the
                // new rows.
                Some(TargetCommand::Update) => {
                    add(self.policy_expressions(rel.relid, 'w', who, false, false));
                    if target_select || rel.scanned {
                        add(self.policy_expressions(rel.relid, 'r', who, false, false));
                    }
                    add(self.policy_expressions(rel.relid, 'w', who, true, false));
                    if target_select {
                        add(self.policy_expressions(rel.relid, 'r', who, true, true));
                    }
                }
                // DELETE: the USING clauses filter the rows it finds, and
                // the SELECT ones when the statement reads the table. An
                // occurrence of the same relation scanned with a row lock
                // (`DELETE ... WHERE id = (SELECT ... FOR UPDATE)`) is an
                // RTE of its own: its UPDATE USING clauses are security
                // quals, as a locked read's are.
                Some(TargetCommand::Delete) => {
                    if rel.scanned && rel.locked {
                        add(self.policy_expressions(rel.relid, 'w', who, false, false));
                    }
                    add(self.policy_expressions(rel.relid, 'd', who, false, false));
                    if target_select || rel.scanned {
                        add(self.policy_expressions(rel.relid, 'r', who, false, false));
                    }
                }
            }
        }
        if recursive {
            if has_sublinks {
                return Err(
                    crate::pgmsg::infinite_policy_recursion(&class.relname).finalize_implicit()
                );
            }
            return Ok(());
        }
        if roots.is_empty() {
            // No permissive policy grants anything: the default-deny `false`
            // runs no policy expression.
            return Ok(());
        }
        // A scan the planner proves empty is a dummy relation: the rewriter
        // still added its security quals, but the planner never builds them,
        // so no policy expression is initialized and no relation a policy's
        // subqueries read is planned. (`check_enable_rls` and the recursion
        // check above happen at rewrite time and still apply.)
        if rel.proven_empty && rel.target.is_none() {
            return Ok(());
        }
        // Each expression PG builds is analyzed on its own: its clauses
        // fold together (an `OR` with a constant `TRUE` drops the others),
        // but one expression's constant never prunes another's. A clause the
        // analyzer can't read falls back to its policy's `pg_depend` rows
        // alone.
        let mut analyzed = Requirements::default();
        let mut unanalyzable: Vec<crate::oid::PgGenericOid> = Vec::new();
        let mut root_nodes: Vec<Node> = Vec::new();
        // The stored clauses' column-notation calls (`(row).f`) resolved to
        // their routines by OID: PG keeps the `FuncExpr`, so a rename or
        // `SET SCHEMA` of the routine leaves the call meaning what it meant.
        let mut indirections: HashMap<(i32, String), PgProcOid> = HashMap::new();
        for policy in self.policies.get(&rel.relid).into_iter().flatten() {
            if !policies.contains(&policy.oid) {
                continue;
            }
            let Some((using, with_check)) = self
                .column_deps
                .policy_quals
                .get(&(rel.relid, policy.name.clone()))
            else {
                continue;
            };
            for clause in using.iter().chain(with_check.iter()) {
                for (key, oid) in &clause.refs.indirections {
                    indirections.insert(key.clone(), *oid);
                }
            }
        }
        let root_nodes_ref = &mut root_nodes;
        crate::ddl::policies::with_indirection_bindings(indirections, || {
            for (expr, oids) in &roots {
                root_nodes_ref.push(expr.clone());
                match self.analyze_over_row(Some(rel.relid), None, &[Some(expr.clone())]) {
                    Some(req) => analyzed.merge(&req),
                    None => {
                        for &oid in oids {
                            if !unanalyzable.contains(&oid) {
                                unanalyzable.push(oid);
                            }
                        }
                    }
                }
            }
        });
        let analyzable: Vec<crate::oid::PgGenericOid> = policies
            .iter()
            .copied()
            .filter(|oid| !unanalyzable.contains(oid))
            .collect();
        // The stored policy resolved its names when it was created: no
        // schema's USAGE is checked at run time, and its functions are the
        // ones its `pg_depend` rows name. What its subqueries read is in the
        // range table — checked as `checkAsUser` (`setRuleCheckAsUser`).
        let mut required = analyzed;
        required.schemas.clear();
        let mut routines: Vec<(PgProcOid, RoutineKind)> = Vec::new();
        self.collect_policy_routines(&analyzable, &root_nodes, &required, &mut routines, true);
        required.routines = routines;
        // The policy's own column reads are not the statement's: only a
        // sublink that scans the table (a relation mark with `scanned`) needs
        // the caller's privileges, and every relation is checked as `who`.
        required
            .relations
            .retain(|r| r.relid != rel.relid || r.scanned);
        for r in &mut required.relations {
            r.check_as = Some(who);
            r.view_owner = rel.check_as.is_some();
        }
        required.executes = executes;
        // PG pushes the relation while it rewrites the policy's subqueries
        // (`activeRIRs`), so a policy that reaches it again is caught.
        if has_sublinks {
            ctx.rls_active.push(rel.relid);
        }
        let result = self
            .check_requirements_in(role, &required, ctx)
            .and_then(|()| {
                if unanalyzable.is_empty() {
                    return Ok(());
                }
                // A clause the analyzer can't read still applies: check what
                // the policy's `pg_depend` rows name.
                self.check_policy_dependencies(role, rel, &unanalyzable, executes, ctx)
            });
        if has_sublinks {
            ctx.rls_active.pop();
        }
        result
    }

    /// A policy expression the analyzer can't analyze (a construct it
    /// doesn't model, an object it doesn't follow through a rename) still
    /// applies to every statement PG's rewriter gives it to. Its `pg_depend`
    /// rows name what it reads by OID: check the routines' EXECUTE and the
    /// relations its subqueries scan, as the role the policy is checked for.
    /// Built-in routines, which `pg_depend` doesn't record, can't be found
    /// without analyzing the expression.
    fn check_policy_dependencies(
        &self,
        role: &str,
        rel: &RelationRequirement,
        policies: &[crate::oid::PgGenericOid],
        executes: bool,
        ctx: &mut BodyContext,
    ) -> Result<(), AnalyzeError> {
        use crate::ddl::depend::PG_POLICY_RELID;
        let Some(who) = rel
            .check_as
            .or_else(|| self.roles.id(role).map(|id| self.roles.canonical(id)))
        else {
            return Ok(());
        };
        let mut required = Requirements {
            executes,
            ..Requirements::default()
        };
        self.collect_policy_routines(
            policies,
            &[],
            &Requirements::default(),
            &mut required.routines,
            false,
        );
        for d in self.pg_depend.iter().filter(|d| {
            d.classid == PG_POLICY_RELID
                && policies.contains(&d.objid)
                && d.objsubid == 0
                && d.deptype == crate::pg_catalog::DepType::Normal
                && d.refclassid == crate::pg_catalog::PG_CLASS_RELID
        }) {
            let Some(relid) = PgClassOid::new(d.refobjid.get()) else {
                continue;
            };
            // The policy's own row: its columns are the row's, not the
            // caller's.
            if relid == rel.relid {
                continue;
            }
            let entry = match required.relations.iter_mut().find(|r| r.relid == relid) {
                Some(entry) => entry,
                None => {
                    required.relations.push(RelationRequirement {
                        relid,
                        check_as: Some(who),
                        perms: bits::SELECT,
                        target: None,
                        conflict_update: false,
                        conflict_select: false,
                        merge_target: false,
                        target_select: false,
                        view_owner: rel.check_as.is_some(),
                        proven_empty: false,
                        no_range_table: false,
                        selected: Vec::new(),
                        inserted: Vec::new(),
                        updated: Vec::new(),
                        locked: false,
                        scanned: true,
                        writes: Vec::new(),
                        only: false,
                        assigned: Vec::new(),
                        insert_leaves: Vec::new(),
                        insert_route_unknown: false,
                    });
                    required.relations.last_mut().expect("just pushed")
                }
            };
            if d.refobjsubid > 0
                && let Some(a) = self
                    .attributes_of(relid)
                    .iter()
                    .find(|a| a.attnum == d.refobjsubid)
            {
                let col = Column::Named(a.attname.clone());
                if !entry.selected.contains(&col) {
                    entry.selected.push(col);
                }
            }
        }
        self.check_requirements_in(role, &required, ctx)
    }

    /// Add the routines the policy expressions run, as the stored objects
    /// name them: the `pg_depend` rows of the policies that contribute a
    /// clause (a `pg_depend` row keeps the functions of dead code too, so
    /// with `prune`, a name the live walk doesn't call is pruned), then the
    /// built-ins the live walk resolves (PG records no dependency on a
    /// pinned object). Without `prune` (an expression the analyzer couldn't
    /// re-analyze) every dependency may run.
    fn collect_policy_routines(
        &self,
        policies: &[crate::oid::PgGenericOid],
        roots: &[Node],
        live: &Requirements,
        routines: &mut Vec<(PgProcOid, RoutineKind)>,
        prune: bool,
    ) {
        use crate::ddl::depend::PG_POLICY_RELID;
        let callees = {
            let mut all = crate::expr::plan_fold::Callees::default();
            for root in roots {
                let one = crate::expr::plan_fold::callees(Some(root), self);
                all.live.extend(one.live);
                all.dead.extend(one.dead);
                all.live_arities.extend(one.live_arities);
                all.dead_arities.extend(one.dead_arities);
            }
            all
        };
        let runs = |name: &str| !prune || callees.runs_mentioned(name);
        let runs_routine =
            |name: &str, nargs: usize| !prune || callees.runs_routine_mentioned(name, nargs);
        let kind_of = |oid: PgProcOid| {
            self.pg_proc
                .get(&oid)
                .map_or(RoutineKind::Function, |p| match p.prokind {
                    ProKind::Aggregate => RoutineKind::Aggregate,
                    ProKind::Procedure => RoutineKind::Procedure,
                    _ => RoutineKind::Function,
                })
        };
        for d in self.pg_depend.iter().filter(|d| {
            d.classid == PG_POLICY_RELID
                && policies.contains(&d.objid)
                && d.objsubid == 0
                && d.deptype == crate::pg_catalog::DepType::Normal
        }) {
            if d.refclassid == crate::pg_catalog::PG_PROC_RELID
                && let Some(oid) = PgProcOid::new(d.refobjid.get())
            {
                // A cast function has no call of its name in the expression
                // (PG records the `FuncExpr` the cast became), so the
                // folding walk cannot prune it.
                let is_cast = self.pg_cast.values().any(|c| c.castfunc == Some(oid));
                let Some(proc) = self.pg_proc.get(&oid) else {
                    continue;
                };
                let nargs = proc.proargtypes.len();
                // A routine the plan runs through a parameter DEFAULT: the
                // stored tree doesn't name it, but the catalog's
                // `RoutineDefault` binds it by OID and the live analysis
                // recorded it when the default certainly runs.
                let from_default =
                    !callees.mentions(&proc.proname) && live.stable_routines.contains(&oid);
                let is_live = is_cast
                    || (callees.mentions(&proc.proname) && runs_routine(&proc.proname, nargs))
                    || from_default;
                if prune && !is_live {
                    continue;
                }
                if !routines.iter().any(|(o, _)| *o == oid) {
                    routines.push((oid, kind_of(oid)));
                }
            } else if d.refclassid == crate::pg_catalog::PG_OPERATOR_RELID
                && let Some(op) = crate::oid::PgOperatorOid::new(d.refobjid.get())
                && let Some((code, name)) = self
                    .pg_operator
                    .get(&op)
                    .and_then(|o| Some((o.oprcode?, o.oprname.as_str())))
            {
                if !runs(name) {
                    continue;
                }
                if !routines.iter().any(|(o, _)| *o == code) {
                    routines.push((code, kind_of(code)));
                }
            }
        }
        // A user function of the same name the expression was created with
        // shadows a built-in the live walk resolves; then the dependency
        // names the function that runs.
        let shadowed: Vec<&str> = self
            .pg_depend
            .iter()
            .filter(|d| {
                d.classid == PG_POLICY_RELID
                    && policies.contains(&d.objid)
                    && d.objsubid == 0
                    && d.deptype == crate::pg_catalog::DepType::Normal
                    && d.refclassid == crate::pg_catalog::PG_PROC_RELID
            })
            .filter_map(|d| PgProcOid::new(d.refobjid.get()))
            .filter(|oid| oid.get() >= crate::ddl::depend::FIRST_UNPINNED_OBJECT_ID)
            .filter_map(|oid| self.pg_proc.get(&oid).map(|p| p.proname.as_str()))
            .collect();
        for &(oid, kind) in &live.routines {
            if (oid.get() < crate::ddl::depend::FIRST_UNPINNED_OBJECT_ID
                || live.stable_routines.contains(&oid))
                && !self
                    .pg_proc
                    .get(&oid)
                    .is_some_and(|p| shadowed.contains(&p.proname.as_str()))
                && !routines.iter().any(|(o, _)| *o == oid)
            {
                routines.push((oid, kind));
            }
        }
    }
}

/// What one `add_security_quals` / `add_with_check_options` call builds: the
/// expressions PG builds (each with the policies whose clauses it combines),
/// the policies that contributed a clause, and whether any of them holds a
/// subquery (`pg_policy.hassublinks`, the rewriter's recursion check).
struct PolicyClauses {
    roots: Vec<(Node, Vec<crate::oid::PgGenericOid>)>,
    oids: Vec<crate::oid::PgGenericOid>,
    has_sublinks: bool,
}

/// An `AND` of `args` (one argument is returned as it is).
fn and_expr(args: Vec<Node>) -> Node {
    bool_expr(typedpg_pg_query::ast::BoolExprType::AndExpr, args)
}

/// An `OR` of `args` (one argument is returned as it is).
fn or_expr(args: Vec<Node>) -> Node {
    bool_expr(typedpg_pg_query::ast::BoolExprType::OrExpr, args)
}

fn bool_expr(kind: typedpg_pg_query::ast::BoolExprType, mut args: Vec<Node>) -> Node {
    if args.len() == 1 {
        return args.pop().expect("one argument");
    }
    Node::BoolExpr(typedpg_pg_query::NodeBox::new(
        typedpg_pg_query::ast::BoolExpr {
            boolop: kind,
            args: args.into_iter().map(Some).collect(),
            location: -1,
        },
    ))
}

/// The caches of one `analyze_as` call's body walk: what was already
/// checked (per routine, effective role, search path and trigger firing),
/// the PL/pgSQL bodies already parsed, and the textual `search_path` in
/// effect, which a body without a `SET` of its own inherits.
#[derive(Default)]
struct BodyContext {
    visited: HashSet<VisitKey>,
    plpgsql: HashMap<PgProcOid, Option<std::sync::Arc<Vec<PlFunction>>>>,
    /// The schemas each compiled PL/pgSQL body's declarations name.
    plpgsql_schemas: HashMap<PgProcOid, Vec<PgNamespaceOid>>,
    search_path: Vec<String>,
    /// The relations whose policy expressions are being expanded, outermost
    /// first (`activeRIRs`): a policy whose subquery reaches one again is
    /// not re-applied (PG rejects that query instead).
    rls_active: Vec<PgClassOid>,
    /// The session's `row_security` GUC (the connecting role's, not the
    /// effective role of a body it runs): off makes PG reject a statement
    /// row security would affect instead of filtering its rows
    /// (`check_enable_rls`).
    row_security: bool,
}

/// Whether a body statement is one the query analyzer reads: the others
/// (utility statements, transaction control) have no privileges of their
/// own. `CALL` does (`ExecuteCallStmt`: EXECUTE on the procedure).
fn is_analyzable(statement: &typedpg_pg_query::ast::Node) -> bool {
    use typedpg_pg_query::ast::Node;
    matches!(
        statement,
        Node::SelectStmt(_)
            | Node::InsertStmt(_)
            | Node::UpdateStmt(_)
            | Node::DeleteStmt(_)
            | Node::MergeStmt(_)
            | Node::CallStmt(_)
    )
}

/// The type a PL/pgSQL datum's declared type names, as the catalog resolves
/// it: `origtypname` when the compiler recorded one (`pg_catalog.int4`, a
/// user type), else `typname` from `pg_catalog` or along the search path.
fn resolve_type_name(
    catalog: &PgCatalog,
    datatype: &typedpg_pg_query::plpgsql::Type,
) -> Option<crate::oid::PgTypeOid> {
    let parts: Vec<String> = datatype
        .origtypname
        .as_ref()
        .map(|tn| {
            tn.names
                .iter()
                .filter_map(|n| crate::ddl::util::node_string(n.as_ref()))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let typname = datatype.typname.as_str();
    let resolve = |schema: Option<&str>, name: &str| {
        catalog.resolve_type_by_name(schema, name).map(|t| t.oid)
    };
    let found = match parts.as_slice() {
        [] => resolve(Some("pg_catalog"), typname).or_else(|| resolve(None, typname)),
        [name] => resolve(None, name).or_else(|| resolve(Some("pg_catalog"), name)),
        [schema, name] => resolve(Some(schema), name),
        // `database.schema.name`: the current database's schemas only.
        [_, schema, name] => resolve(Some(schema), name),
        _ => None,
    };
    found.or_else(|| resolve(Some("pg_catalog"), typname).or_else(|| resolve(None, typname)))
}

/// The values a trigger's `TG_*` variables have for one firing, which a
/// condition on them is evaluated against. Only the variables the firing
/// determines are known: `TG_NAME` and `TG_NARGS` depend on the trigger's
/// arguments, which the catalog doesn't keep — an unknown `TG_*` is
/// UNKNOWN, never NULL.
#[derive(Clone)]
struct TgValues {
    name: String,
    op: &'static str,
    when_: &'static str,
    level: &'static str,
    table_name: String,
    table_schema: String,
    /// The PL/pgSQL compiler's simple-CASE variable (`__Case__Variable_16__`)
    /// and the value it holds, when the CASE's test expression is known.
    vars: Vec<(String, String)>,
    /// `TG_*` variables the body assigns somewhere (`TG_OP := …`): after
    /// such an assignment the variable doesn't hold the firing's value any
    /// more, so it is UNKNOWN for pruning the whole body (may-run policy).
    unknown: Vec<String>,
}

impl TgValues {
    fn of(firing: &TriggerFiring<'_>, catalog: &PgCatalog) -> Self {
        let class = catalog.pg_class.get(&firing.relid);
        TgValues {
            name: firing.trigger.name.clone(),
            op: crate::ddl::triggers::event_word(firing.event),
            when_: crate::ddl::triggers::timing_word(firing.trigger.timing),
            level: if firing.trigger.row {
                "ROW"
            } else {
                "STATEMENT"
            },
            table_name: class.map(|c| c.relname.clone()).unwrap_or_default(),
            table_schema: class
                .and_then(|c| catalog.namespace_name(c.relnamespace))
                .unwrap_or_default()
                .to_owned(),
            vars: Vec::new(),
            unknown: Vec::new(),
        }
    }

    /// Forget the variables `function` assigns anywhere in its body (an
    /// assignment, `SELECT … INTO tg_…`, a loop variable, `GET
    /// DIAGNOSTICS`): the assignment may have run before any use.
    fn ignore_assigned(&mut self, function: &PlFunction) {
        let mut assigned = Vec::new();
        if let Some(action) = function.action.as_ref() {
            collect_assigned_variables(action, &function.datums, &mut assigned);
        }
        for name in assigned {
            if !self.unknown.contains(&name) {
                self.unknown.push(name);
            }
        }
    }

    /// The value of the variable `name` (case-insensitively): a known
    /// `TG_*` value, or a simple CASE's compiler variable.
    fn get(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        if self.unknown.contains(&name) {
            return None;
        }
        if let Some((_, value)) = self.vars.iter().find(|(n, _)| *n == name) {
            return Some(value);
        }
        match name.as_str() {
            "tg_name" => Some(&self.name),
            "tg_op" => Some(self.op),
            "tg_when" => Some(self.when_),
            "tg_level" => Some(self.level),
            "tg_table_name" | "tg_relname" => Some(&self.table_name),
            "tg_table_schema" => Some(&self.table_schema),
            _ => None,
        }
    }
}

type PlFunction = typedpg_pg_query::plpgsql::Function;
type PlDatum = typedpg_pg_query::plpgsql::Datum;
type PlStmt = typedpg_pg_query::plpgsql::Stmt;
type PlBlock = typedpg_pg_query::plpgsql::StmtBlock;
type PlExpr = typedpg_pg_query::plpgsql::Expr;

/// The datum numbered `dno`.
fn plpgsql_datum(datums: &[PlDatum], dno: i32) -> Option<&PlDatum> {
    datums.get(usize::try_from(dno).ok()?)
}

/// Visit every statement of `stmts`, nested bodies and exception handlers
/// included, before its children.
fn walk_plpgsql_stmts<'a>(stmts: &'a [PlStmt], f: &mut impl FnMut(&'a PlStmt)) {
    for stmt in stmts {
        f(stmt);
        match stmt {
            PlStmt::Block(b) => walk_plpgsql_block(b, f),
            PlStmt::If(s) => {
                walk_plpgsql_stmts(&s.then_body, f);
                for elsif in &s.elsif_list {
                    walk_plpgsql_stmts(&elsif.stmts, f);
                }
                walk_plpgsql_stmts(&s.else_body, f);
            }
            PlStmt::Case(s) => {
                for when in &s.case_when_list {
                    walk_plpgsql_stmts(&when.stmts, f);
                }
                walk_plpgsql_stmts(&s.else_stmts, f);
            }
            PlStmt::Loop(s) => walk_plpgsql_stmts(&s.body, f),
            PlStmt::While(s) => walk_plpgsql_stmts(&s.body, f),
            PlStmt::Fori(s) => walk_plpgsql_stmts(&s.body, f),
            PlStmt::Fors(s) => walk_plpgsql_stmts(&s.body, f),
            PlStmt::Forc(s) => walk_plpgsql_stmts(&s.body, f),
            PlStmt::ForeachA(s) => walk_plpgsql_stmts(&s.body, f),
            PlStmt::Dynfors(s) => walk_plpgsql_stmts(&s.body, f),
            _ => {}
        }
    }
}

/// [`walk_plpgsql_stmts`] over a block's body and its exception handlers.
fn walk_plpgsql_block<'a>(block: &'a PlBlock, f: &mut impl FnMut(&'a PlStmt)) {
    walk_plpgsql_stmts(&block.body, f);
    for exception in block.exceptions.iter().flat_map(|e| &e.exc_list) {
        walk_plpgsql_stmts(&exception.action, f);
    }
}

/// The SQL expressions statement `stmt` itself carries (not its nested
/// statements'), dynamic SQL's string expression and `USING` arguments
/// included.
fn plpgsql_stmt_exprs(stmt: &PlStmt) -> Vec<&PlExpr> {
    let mut out: Vec<&PlExpr> = Vec::new();
    match stmt {
        PlStmt::Assign(s) => out.extend(s.expr.as_ref()),
        PlStmt::If(s) => {
            out.extend(s.cond.as_ref());
            out.extend(s.elsif_list.iter().filter_map(|e| e.cond.as_ref()));
        }
        PlStmt::Case(s) => {
            out.extend(s.t_expr.as_ref());
            out.extend(s.case_when_list.iter().filter_map(|w| w.expr.as_ref()));
        }
        PlStmt::While(s) => out.extend(s.cond.as_ref()),
        PlStmt::Fori(s) => {
            out.extend(s.lower.as_ref());
            out.extend(s.upper.as_ref());
            out.extend(s.step.as_ref());
        }
        PlStmt::Fors(s) => out.extend(s.query.as_ref()),
        PlStmt::Forc(s) => out.extend(s.argquery.as_ref()),
        PlStmt::ForeachA(s) => out.extend(s.expr.as_ref()),
        PlStmt::Exit(s) => out.extend(s.cond.as_ref()),
        PlStmt::Return(s) => out.extend(s.expr.as_ref()),
        PlStmt::ReturnNext(s) => out.extend(s.expr.as_ref()),
        PlStmt::ReturnQuery(s) => {
            out.extend(s.query.as_ref());
            out.extend(s.dynquery.as_ref());
            out.extend(&s.params);
        }
        PlStmt::Raise(s) => {
            out.extend(&s.params);
            out.extend(s.options.iter().filter_map(|o| o.expr.as_ref()));
        }
        PlStmt::Assert(s) => {
            out.extend(s.cond.as_ref());
            out.extend(s.message.as_ref());
        }
        PlStmt::Execsql(s) => out.extend(s.sqlstmt.as_ref()),
        PlStmt::Dynexecute(s) => {
            out.extend(s.query.as_ref());
            out.extend(&s.params);
        }
        PlStmt::Dynfors(s) => {
            out.extend(s.query.as_ref());
            out.extend(&s.params);
        }
        PlStmt::Open(s) => {
            out.extend(s.argquery.as_ref());
            out.extend(s.query.as_ref());
            out.extend(s.dynquery.as_ref());
            out.extend(&s.params);
        }
        PlStmt::Fetch(s) => out.extend(s.expr.as_ref()),
        PlStmt::Perform(s) => out.extend(s.expr.as_ref()),
        PlStmt::Call(s) => out.extend(s.expr.as_ref()),
        PlStmt::Block(_)
        | PlStmt::Loop(_)
        | PlStmt::Getdiag(_)
        | PlStmt::Close(_)
        | PlStmt::Commit(_)
        | PlStmt::Rollback(_) => {}
    }
    out
}

/// The `TG_*` variables `block` assigns anywhere: an assignment's variable,
/// or the target of a `SELECT … INTO` / `FETCH … INTO` / `GET DIAGNOSTICS`
/// / loop, which names the datum written.
fn collect_assigned_variables(block: &PlBlock, datums: &[PlDatum], out: &mut Vec<String>) {
    walk_plpgsql_block(block, &mut |stmt| {
        let mut varnos: Vec<i32> = Vec::new();
        match stmt {
            PlStmt::Assign(s) => varnos.push(s.varno),
            PlStmt::Execsql(s) if s.into => target_varnos(s.target, datums, &mut varnos),
            PlStmt::Dynexecute(s) if s.into => target_varnos(s.target, datums, &mut varnos),
            PlStmt::Fetch(s) => target_varnos(s.target, datums, &mut varnos),
            PlStmt::Fors(s) => target_varnos(s.var, datums, &mut varnos),
            PlStmt::Forc(s) => target_varnos(s.var, datums, &mut varnos),
            PlStmt::Dynfors(s) => target_varnos(s.var, datums, &mut varnos),
            PlStmt::Fori(s) => target_varnos(s.var, datums, &mut varnos),
            PlStmt::ForeachA(s) => target_varnos(Some(s.varno), datums, &mut varnos),
            PlStmt::Getdiag(s) => varnos.extend(s.diag_items.iter().map(|item| item.target)),
            _ => {}
        }
        for varno in varnos {
            if let Some(name) = plpgsql_datum(datums, varno).and_then(PlDatum::refname)
                && name.to_ascii_lowercase().starts_with("tg_")
            {
                out.push(name.to_ascii_lowercase());
            }
        }
    });
}

/// The variable numbers an assignment target writes: a variable or record
/// itself, or each field of a row.
fn target_varnos(target: Option<i32>, datums: &[PlDatum], out: &mut Vec<i32>) {
    let Some(target) = target else {
        return;
    };
    match plpgsql_datum(datums, target) {
        Some(PlDatum::Var(_) | PlDatum::Promise(_) | PlDatum::Rec(_)) => out.push(target),
        Some(PlDatum::Row(row)) => out.extend(row.fields.iter().map(|f| f.varno)),
        _ => {}
    }
}

/// Whether a raise at this level leaves the block: ERROR (elog.h's 21) and
/// above.
const ELOG_ERROR: i32 = 21;

/// Whether a `BEGIN ... EXCEPTION` block catches a permission denial, so
/// that its body's denials are handled, not raised (see this module's
/// policy): `insufficient_privilege` (SQLSTATE `'42501'`), its class
/// `syntax_error_or_access_rule_violation` (`'42000'`) or `OTHERS` — and the
/// handler doesn't re-raise it, nor does the body fire a deferrable
/// constraint trigger whose denial only surfaces at commit, outside the
/// block.
fn block_catches_denial(block: &PlBlock, catalog: &PgCatalog) -> bool {
    let Some(exceptions) = block.exceptions.as_ref() else {
        return false;
    };
    let catches = |exception: &typedpg_pg_query::plpgsql::Exception| {
        exception.conditions.iter().any(|condition| {
            let name = condition.condname.as_str();
            // The condition, its class (`42000`) or OTHERS.
            name.eq_ignore_ascii_case("insufficient_privilege")
                || name == "42501"
                || name.eq_ignore_ascii_case("syntax_error_or_access_rule_violation")
                || name == "42000"
                || name.eq_ignore_ascii_case("others")
        })
    };
    // A handler that may re-raise somewhere doesn't handle the denial: it
    // leaves the block (`RAISE;`, `RAISE EXCEPTION …`, anything at ERROR).
    let catching: Vec<&typedpg_pg_query::plpgsql::Exception> =
        exceptions.exc_list.iter().filter(|e| catches(e)).collect();
    if catching.is_empty() {
        return false;
    }
    // A deferred constraint trigger the body fires raises at commit, after
    // the block is gone: no handler catches that.
    if body_fires_deferred_trigger(block, catalog) {
        return false;
    }
    catching
        .iter()
        .any(|exception| !stmts_reraise(&exception.action))
}

/// Whether the statements hold a `RAISE` at ERROR level (or above)
/// anywhere: a re-raise the enclosing handler doesn't swallow.
fn stmts_reraise(stmts: &[PlStmt]) -> bool {
    let mut found = false;
    walk_plpgsql_stmts(stmts, &mut |stmt| {
        if let PlStmt::Raise(raise) = stmt
            && raise.elog_level >= ELOG_ERROR
        {
            found = true;
        }
    });
    found
}

/// Whether the block's statements may write a relation with a deferrable
/// constraint trigger: its check runs at commit (or a later `SET
/// CONSTRAINTS`), outside every enclosing exception handler.
fn body_fires_deferred_trigger(block: &PlBlock, catalog: &PgCatalog) -> bool {
    let mut queries: Vec<&str> = Vec::new();
    walk_plpgsql_block(block, &mut |stmt| {
        queries.extend(
            plpgsql_stmt_exprs(stmt)
                .into_iter()
                .map(|e| e.query.as_str()),
        );
    });
    queries
        .iter()
        .any(|query| statement_writes_deferrable_trigger(catalog, query))
}
/// Whether `query` writes (`INSERT` / `UPDATE` / `DELETE` / `MERGE`) a
/// relation with a constraint trigger that certainly runs at commit
/// (`DEFERRABLE INITIALLY DEFERRED`) for the command's event.
fn statement_writes_deferrable_trigger(catalog: &PgCatalog, query: &str) -> bool {
    let Ok(parsed) = typedpg_pg_query::parse(query) else {
        return false;
    };
    let Some(statement) = parsed.tree.stmts.first().and_then(|r| r.stmt.as_ref()) else {
        return false;
    };
    use crate::ddl::triggers::{TRIGGER_TYPE_DELETE, TRIGGER_TYPE_INSERT, TRIGGER_TYPE_UPDATE};
    use typedpg_pg_query::ast::CmdType;
    use typedpg_pg_query::ast::Node;
    let (relation, command) = match statement {
        Node::InsertStmt(s) => (s.relation.as_ref(), Some(CmdType::CmdInsert)),
        Node::UpdateStmt(s) => (s.relation.as_ref(), Some(CmdType::CmdUpdate)),
        Node::DeleteStmt(s) => (s.relation.as_ref(), Some(CmdType::CmdDelete)),
        // MERGE may insert, update or delete: any of the three.
        Node::MergeStmt(s) => (s.relation.as_ref(), None),
        _ => (None, None),
    };
    let Some(relation) = relation else {
        return false;
    };
    let event_of = |command: Option<CmdType>| match command {
        Some(CmdType::CmdInsert) => TRIGGER_TYPE_INSERT,
        Some(CmdType::CmdUpdate) => TRIGGER_TYPE_UPDATE,
        Some(CmdType::CmdDelete) => TRIGGER_TYPE_DELETE,
        // MERGE: any row event.
        _ => TRIGGER_TYPE_INSERT | TRIGGER_TYPE_UPDATE | TRIGGER_TYPE_DELETE,
    };
    let event = event_of(command);
    let schema = (!relation.schemaname.is_empty()).then_some(relation.schemaname.as_str());
    // A body statement's names are resolved for this check only: the write
    // itself requires the schema's USAGE when the statement is checked, and
    // this one may be pruned (a caught block's body).
    let Some(class) =
        crate::privileges::suspended(|| catalog.resolve_table(schema, &relation.relname))
    else {
        return false;
    };
    let mut todo = vec![class.oid];
    let mut seen = HashSet::new();
    while let Some(relid) = todo.pop() {
        if !seen.insert(relid) {
            continue;
        }
        if catalog.triggers.get(&relid).is_some_and(|triggers| {
            triggers.iter().any(|t| {
                t.constraint_deferrable == Some(true)
                    && t.constraint_deferred == Some(true)
                    && t.state.fires()
                    && t.events & event != 0
            })
        }) {
            return true;
        }
        todo.extend(
            catalog
                .pg_inherits
                .iter()
                .filter(|i| i.inhparent == relid)
                .map(|i| i.inhrelid),
        );
    }
    false
}

/// Where a record variable's shape comes from.
#[derive(Clone, PartialEq, Eq)]
enum ShapeSource {
    /// A query whose columns the record takes (`SELECT … INTO`, `FOR … IN`).
    Query(String),
    /// Another record variable (or NEW / OLD): its shape is copied.
    Record(String),
}

/// The shapes each record variable may have at a point of the body: a
/// record assigned on several paths has several (`r` gets one shape in an
/// `IF` branch, another in the `ELSE`), and a statement reading it is
/// analyzed for each.
type ShapeEnv = HashMap<String, Vec<ShapeSource>>;

/// Every record `env` knows: `env`'s own plus what its copied records carry.
fn merge_shape_env(into: &mut ShapeEnv, other: &ShapeEnv) {
    for (name, sources) in other {
        let entry = into.entry(name.clone()).or_default();
        for source in sources {
            if !entry.contains(source) {
                entry.push(source.clone());
            }
        }
    }
}

/// Push the query an expression carries, when code is being run.
fn collect_expr(
    expr: Option<&PlExpr>,
    run: bool,
    env: &ShapeEnv,
    out: &mut Vec<(String, ShapeEnv)>,
) {
    if !run {
        return;
    }
    if let Some(expr) = expr {
        out.push((expr.query.clone(), env.clone()));
    }
}

/// Push the queries a list of expressions carries.
fn collect_exprs(exprs: &[PlExpr], run: bool, env: &ShapeEnv, out: &mut Vec<(String, ShapeEnv)>) {
    for expr in exprs {
        collect_expr(Some(expr), run, env, out);
    }
}

/// The `(record, source)` of a datum a statement fills from a query's
/// columns: an `INTO` target of `SELECT` / `RETURNING`.
fn into_shape(
    statement: &typedpg_pg_query::plpgsql::StmtExecsql,
    datums: &[PlDatum],
    env: &mut ShapeEnv,
) {
    if !statement.into {
        return;
    }
    let Some(PlDatum::Rec(rec)) = statement.target.and_then(|t| plpgsql_datum(datums, t)) else {
        return;
    };
    if let Some(query) = statement.sqlstmt.as_ref() {
        env.entry(rec.refname.clone())
            .or_default()
            .push(ShapeSource::Query(query.query.clone()));
    }
}

/// The `(record, source)` of a `FOR record IN query` loop, whose record takes
/// the query's columns' shape.
fn for_shape(
    statement: &typedpg_pg_query::plpgsql::StmtFors,
    datums: &[PlDatum],
    env: &mut ShapeEnv,
) {
    let Some(name) = statement
        .var
        .and_then(|v| plpgsql_datum(datums, v))
        .and_then(PlDatum::refname)
    else {
        return;
    };
    if let Some(query) = statement.query.as_ref() {
        env.entry(name.to_owned())
            .or_default()
            .push(ShapeSource::Query(query.query.clone()));
    }
}

/// The `(record, source)` of a `record := source` assignment: `NEW` / `OLD`,
/// another record or row variable, copies its shape.
fn assign_shape(
    statement: &typedpg_pg_query::plpgsql::StmtAssign,
    datums: &[PlDatum],
    env: &mut ShapeEnv,
) {
    let Some(PlDatum::Rec(rec)) = plpgsql_datum(datums, statement.varno) else {
        return;
    };
    let Some(query) = statement.expr.as_ref().map(|e| e.query.as_str()) else {
        return;
    };
    let Some((_, rhs)) = query.split_once(":=") else {
        return;
    };
    let rhs = rhs.trim();
    if is_identifier(rhs) {
        env.entry(rec.refname.clone())
            .or_default()
            .push(ShapeSource::Record(rhs.to_ascii_lowercase()));
    }
}

/// Whether `text` is a single identifier (an assignment's record source).
fn is_identifier(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
}

/// What a walk of a body's statements reads: the firing's `TG_*` values, the
/// function's variables and the catalog.
#[derive(Clone, Copy)]
struct PlWalk<'a> {
    tg: Option<&'a TgValues>,
    datums: &'a [PlDatum],
    catalog: &'a PgCatalog,
}

/// A statement list: each statement in turn, up to one that ends the block
/// (an unconditional `RETURN` / `RAISE EXCEPTION`, after which nothing in
/// the list runs).
fn walk_plpgsql_statements(
    statements: &[PlStmt],
    walk: PlWalk<'_>,
    env: &mut ShapeEnv,
    run: bool,
    out: &mut Vec<(String, ShapeEnv)>,
) {
    for statement in statements {
        walk_plpgsql_statement(statement, walk, env, run, out);
        if ends_the_block(statement) {
            break;
        }
    }
}

/// Whether `statement` transfers control out of the block for good.
fn ends_the_block(statement: &PlStmt) -> bool {
    match statement {
        PlStmt::Return(_) => true,
        PlStmt::Raise(raise) => raise.elog_level >= ELOG_ERROR,
        // A nested block without an EXCEPTION clause whose body ends
        // unconditionally ends the enclosing list too.
        PlStmt::Block(block) => block.exceptions.is_none() && list_ends_the_block(&block.body),
        // An IF whose every branch, ELSE included, ends.
        PlStmt::If(s) => {
            list_ends_the_block(&s.then_body)
                && s.elsif_list.iter().all(|e| list_ends_the_block(&e.stmts))
                && list_ends_the_block(&s.else_body)
        }
        _ => false,
    }
}

/// Whether a statement list ends unconditionally: some statement in it ends
/// the block (nothing after that one runs).
fn list_ends_the_block(statements: &[PlStmt]) -> bool {
    statements.iter().any(ends_the_block)
}

/// The query a PL/pgSQL cursor variable number declares, if it is a cursor.
fn cursor_query(datums: &[PlDatum], varno: i32) -> Option<String> {
    let Some(PlDatum::Var(var)) = plpgsql_datum(datums, varno) else {
        return None;
    };
    var.cursor_explicit_expr.as_ref().map(|e| e.query.clone())
}

/// Walk a loop body: one iteration first, to learn the shapes it may leave
/// behind, then again from the merged state — a statement at the top of the
/// body may see what an earlier iteration's bottom assigned. Only the
/// queries of the second walk are kept.
fn walk_loop_body(
    statements: &[PlStmt],
    walk: PlWalk<'_>,
    env: &mut ShapeEnv,
    run: bool,
    out: &mut Vec<(String, ShapeEnv)>,
) {
    let entry = env.clone();
    let mut first = entry.clone();
    let before = out.len();
    walk_plpgsql_statements(statements, walk, &mut first, run, out);
    out.truncate(before);
    let mut merged = entry;
    merge_shape_env(&mut merged, &first);
    let mut second = merged.clone();
    walk_plpgsql_statements(statements, walk, &mut second, run, out);
    let mut exit = merged;
    merge_shape_env(&mut exit, &second);
    *env = exit;
}

/// A block: its `DECLARE`d variables' defaults, its body (pruned when its
/// handler catches a permission denial) and its handlers.
fn walk_plpgsql_block_statement(
    block: &PlBlock,
    walk: PlWalk<'_>,
    env: &mut ShapeEnv,
    run: bool,
    out: &mut Vec<(String, ShapeEnv)>,
) {
    let caught = block_catches_denial(block, walk.catalog);
    // The block's DECLAREd variables run their defaults when it is entered
    // — before `exec_stmt_block` opens the handler's protected
    // subtransaction: a denial in one isn't caught by the block's own
    // handlers.
    for &varno in &block.initvarnos {
        if let Some(PlDatum::Var(var)) = plpgsql_datum(walk.datums, varno) {
            collect_expr(var.default_val.as_ref(), run, env, out);
        }
    }
    walk_plpgsql_statements(&block.body, walk, env, run && !caught, out);
    // The handler's own statements are still checked (a handler that may
    // re-raise was not taken for a catcher above). A handler may run with
    // the state the block had when the error was raised; the statements'
    // own assignments stay.
    for exception in block.exceptions.iter().flat_map(|e| &e.exc_list) {
        walk_plpgsql_statements(&exception.action, walk, env, run, out);
    }
}

/// One PL/pgSQL statement: push the queries it runs, and recurse into the
/// statements it runs (see this module's policy for what is pruned), along
/// with the record shapes each query sees when it runs.
fn walk_plpgsql_statement(
    statement: &PlStmt,
    walk: PlWalk<'_>,
    env: &mut ShapeEnv,
    run: bool,
    out: &mut Vec<(String, ShapeEnv)>,
) {
    let PlWalk {
        tg,
        datums,
        catalog,
    } = walk;
    match statement {
        PlStmt::Block(block) => walk_plpgsql_block_statement(block, walk, env, run, out),
        PlStmt::If(s) => {
            // The condition is evaluated (and so planned, with its
            // relations' privileges checked) even when its value is known.
            let condition = evaluated_condition(s.cond.as_ref(), tg, catalog, env, run, out);
            let entry = env.clone();
            let mut branches: Vec<ShapeEnv> = Vec::new();
            if condition != Some(false) {
                let mut then_env = entry.clone();
                walk_plpgsql_statements(&s.then_body, walk, &mut then_env, run, out);
                branches.push(then_env);
            }
            // A known true takes the THEN branch for good; otherwise the
            // ELSIFs and the ELSE may run.
            if condition != Some(true) {
                let mut taken = false;
                for elsif in &s.elsif_list {
                    let condition =
                        evaluated_condition(elsif.cond.as_ref(), tg, catalog, env, run, out);
                    if condition == Some(false) {
                        continue;
                    }
                    let mut elsif_env = entry.clone();
                    walk_plpgsql_statements(&elsif.stmts, walk, &mut elsif_env, run, out);
                    branches.push(elsif_env);
                    if condition == Some(true) {
                        taken = true;
                        break;
                    }
                }
                if !taken {
                    let mut else_env = entry.clone();
                    walk_plpgsql_statements(&s.else_body, walk, &mut else_env, run, out);
                    branches.push(else_env);
                }
            }
            // Whatever a branch assigned may be there after the IF.
            let mut merged = entry;
            for branch in branches {
                merge_shape_env(&mut merged, &branch);
            }
            *env = merged;
        }
        PlStmt::Case(s) => {
            // A searched CASE tests each condition. A simple CASE compares
            // its subject with each WHEN; when the subject is a known TG_*
            // value, the compiler's comparison folds (`TG_OP = 'DELETE'`).
            let simple = s.t_expr.is_some();
            let mut case_tg = None;
            if simple {
                collect_expr(s.t_expr.as_ref(), run, env, out);
                let value = s
                    .t_expr
                    .as_ref()
                    .and_then(|e| known_string_query(&e.query, tg));
                if let (Some(tg), Some(value)) = (tg, value)
                    && let Some(PlDatum::Var(var)) = plpgsql_datum(datums, s.t_varno)
                {
                    let mut tg = tg.clone();
                    tg.vars.push((var.refname.to_ascii_lowercase(), value));
                    case_tg = Some(tg);
                }
            }
            let entry = env.clone();
            let mut branches: Vec<ShapeEnv> = Vec::new();
            let mut taken = false;
            for when in &s.case_when_list {
                collect_expr(when.expr.as_ref(), run, env, out);
                let condition = if simple {
                    condition_value(when.expr.as_ref(), case_tg.as_ref().or(tg), catalog)
                } else {
                    evaluated_condition(when.expr.as_ref(), tg, catalog, env, run, out)
                };
                if condition == Some(false) {
                    continue;
                }
                let mut when_env = entry.clone();
                walk_plpgsql_statements(&when.stmts, walk, &mut when_env, run, out);
                branches.push(when_env);
                if condition == Some(true) {
                    taken = true;
                    break;
                }
            }
            if !taken && s.have_else {
                let mut else_env = entry.clone();
                walk_plpgsql_statements(&s.else_stmts, walk, &mut else_env, run, out);
                branches.push(else_env);
            }
            let mut merged = entry;
            for branch in branches {
                merge_shape_env(&mut merged, &branch);
            }
            *env = merged;
        }
        PlStmt::While(s) => {
            if evaluated_condition(s.cond.as_ref(), tg, catalog, env, run, out) != Some(false) {
                walk_loop_body(&s.body, walk, env, run, out);
            }
        }
        PlStmt::Fori(s) => {
            collect_expr(s.lower.as_ref(), run, env, out);
            collect_expr(s.upper.as_ref(), run, env, out);
            collect_expr(s.step.as_ref(), run, env, out);
            walk_loop_body(&s.body, walk, env, run, out);
        }
        PlStmt::Exit(s) => collect_expr(s.cond.as_ref(), run, env, out),
        PlStmt::Open(s) => {
            // A cursor's declared query runs when it is OPENed, not when
            // its block is entered; an unbound refcursor's own query (or
            // the expression producing the dynamic one) runs then too.
            if let Some(query) = cursor_query(datums, s.curvar)
                && run
            {
                out.push((query, env.clone()));
            }
            collect_expr(s.query.as_ref(), run, env, out);
            collect_expr(s.dynquery.as_ref(), run, env, out);
            collect_expr(s.argquery.as_ref(), run, env, out);
        }
        PlStmt::Forc(s) => {
            if let Some(query) = cursor_query(datums, s.curvar)
                && run
            {
                out.push((query, env.clone()));
            }
            collect_expr(s.argquery.as_ref(), run, env, out);
            walk_loop_body(&s.body, walk, env, run, out);
        }
        PlStmt::ForeachA(s) => {
            collect_expr(s.expr.as_ref(), run, env, out);
            walk_loop_body(&s.body, walk, env, run, out);
        }
        PlStmt::Loop(s) => walk_loop_body(&s.body, walk, env, run, out),
        PlStmt::Fors(s) => {
            for_shape(s, datums, env);
            collect_expr(s.query.as_ref(), run, env, out);
            walk_loop_body(&s.body, walk, env, run, out);
        }
        PlStmt::Execsql(s) => {
            into_shape(s, datums, env);
            collect_expr(s.sqlstmt.as_ref(), run, env, out);
        }
        PlStmt::Assign(s) => {
            assign_shape(s, datums, env);
            collect_expr(s.expr.as_ref(), run, env, out);
        }
        PlStmt::Perform(s) => collect_expr(s.expr.as_ref(), run, env, out),
        PlStmt::Call(s) => collect_expr(s.expr.as_ref(), run, env, out),
        PlStmt::Return(s) => collect_expr(s.expr.as_ref(), run, env, out),
        PlStmt::ReturnNext(s) => collect_expr(s.expr.as_ref(), run, env, out),
        PlStmt::ReturnQuery(s) => {
            collect_expr(s.query.as_ref(), run, env, out);
            // `RETURN QUERY EXECUTE <expr> [USING …]`: the expression
            // producing the query string runs before the query is prepared.
            collect_expr(s.dynquery.as_ref(), run, env, out);
            collect_exprs(&s.params, run, env, out);
        }
        // Dynamic SQL's string isn't known: only the expression producing
        // it and its `USING` expressions run now.
        PlStmt::Dynexecute(s) => {
            collect_expr(s.query.as_ref(), run, env, out);
            collect_exprs(&s.params, run, env, out);
        }
        // `FOR r IN EXECUTE … LOOP`: the query string is unknown, but the
        // loop's static body runs.
        PlStmt::Dynfors(s) => {
            collect_expr(s.query.as_ref(), run, env, out);
            collect_exprs(&s.params, run, env, out);
            walk_loop_body(&s.body, walk, env, run, out);
        }
        PlStmt::Raise(s) => collect_exprs(&s.params, run, env, out),
        PlStmt::Assert(s) => {
            collect_expr(s.cond.as_ref(), run, env, out);
            collect_expr(s.message.as_ref(), run, env, out);
        }
        PlStmt::Fetch(_)
        | PlStmt::Getdiag(_)
        | PlStmt::Close(_)
        | PlStmt::Commit(_)
        | PlStmt::Rollback(_) => {}
    }
}

/// Whether a PL/pgSQL condition is known to be true, or not true, from its
/// literal constants, the planner's constant folding and the firing's
/// `TG_*` values. `None`: it depends on data.
fn condition_value(
    condition: Option<&PlExpr>,
    tg: Option<&TgValues>,
    catalog: &PgCatalog,
) -> Option<bool> {
    let expression = parse_condition(&condition?.query)?;
    match boolean_value(Some(&expression), tg) {
        Some(Tri::True) => return Some(true),
        Some(Tri::False | Tri::Null) => return Some(false),
        Some(Tri::Unknown) | None => {}
    }
    // PL/pgSQL plans its conditions as `SELECT <expr>`: the planner's
    // certainty folds constants (`IF 1 = 0`, `IF NULL::boolean`) before the
    // branch runs.
    match crate::expr::plan_fold::certainly(Some(&expression), catalog) {
        crate::expr::plan_fold::Certain::Bool(true) => Some(true),
        crate::expr::plan_fold::Certain::Bool(false) | crate::expr::plan_fold::Certain::Null => {
            Some(false)
        }
        crate::expr::plan_fold::Certain::Value
        | crate::expr::plan_fold::Certain::UnknownValue
        | crate::expr::plan_fold::Certain::Not => None,
    }
}

/// A condition the statement evaluates: push its own SQL — it is planned
/// and run, so its relations are checked — and report its known value.
fn evaluated_condition(
    condition: Option<&PlExpr>,
    tg: Option<&TgValues>,
    catalog: &PgCatalog,
    env: &ShapeEnv,
    run: bool,
    out: &mut Vec<(String, ShapeEnv)>,
) -> Option<bool> {
    collect_expr(condition, run, env, out);
    condition_value(condition, tg, catalog)
}

/// The string `query` (an expression) is known to hold, for a simple CASE's
/// test.
fn known_string_query(query: &str, tg: Option<&TgValues>) -> Option<String> {
    let expression = parse_condition(query)?;
    string_value(Some(&expression), tg)?
}

/// Parse an expression written on its own as the value of `SELECT
/// expression`.
fn parse_condition(query: &str) -> Option<ast::Node> {
    let parsed = typedpg_pg_query::parse(&format!("SELECT {query}")).ok()?;
    let [raw] = parsed.tree.stmts.as_slice() else {
        return None;
    };
    let Some(ast::Node::SelectStmt(select)) = raw.stmt.as_ref() else {
        return None;
    };
    let Some(ast::Node::ResTarget(target)) = select.target_list.first()?.as_ref() else {
        return None;
    };
    target.val.clone()
}

/// SQL's three-valued boolean logic, plus "the expression isn't a constant
/// test at all".
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tri {
    True,
    False,
    Null,
    Unknown,
}

impl Tri {
    fn not(self) -> Tri {
        match self {
            Tri::True => Tri::False,
            Tri::False => Tri::True,
            Tri::Null => Tri::Null,
            Tri::Unknown => Tri::Unknown,
        }
    }
}

/// Evaluate an expression made of literals and `TG_*` comparisons.
fn boolean_value(expression: Option<&ast::Node>, tg: Option<&TgValues>) -> Option<Tri> {
    use typedpg_pg_query::ast::BoolExprType;
    let value = match expression? {
        ast::Node::AConst(c) => match c.val.as_ref() {
            Some(ast::AConstValue::Boolean(b)) => {
                if b.boolval {
                    Tri::True
                } else {
                    Tri::False
                }
            }
            _ if c.isnull => Tri::Null,
            _ => Tri::Unknown,
        },
        // `true::boolean`, `'INSERT'::text`: only a cast that doesn't change
        // the value — a domain coercion or a typed conversion is never
        // folded here (see `plan_fold::certainly`).
        ast::Node::TypeCast(cast) => {
            if !boolean_cast_is_plain(cast) {
                return None;
            }
            boolean_value(cast.arg.as_ref(), tg)?
        }
        ast::Node::BoolExpr(expr) => {
            let args: Vec<Tri> = expr
                .args
                .iter()
                .map(|a| boolean_value(a.as_ref(), tg).unwrap_or(Tri::Unknown))
                .collect();
            match expr.boolop {
                BoolExprType::AndExpr => match args.iter() {
                    _ if args.contains(&Tri::False) => Tri::False,
                    _ if args.contains(&Tri::Unknown) => Tri::Unknown,
                    _ if args.contains(&Tri::Null) => Tri::Null,
                    _ => Tri::True,
                },
                BoolExprType::OrExpr => match args.iter() {
                    _ if args.contains(&Tri::True) => Tri::True,
                    _ if args.contains(&Tri::Unknown) => Tri::Unknown,
                    _ if args.contains(&Tri::Null) => Tri::Null,
                    _ => Tri::False,
                },
                BoolExprType::NotExpr => match *args.as_slice() {
                    [only] => only.not(),
                    _ => Tri::Unknown,
                },
            }
        }
        ast::Node::AExpr(expr) => compare_value(expr, tg)?,
        ast::Node::NullTest(test) => {
            use typedpg_pg_query::ast::NullTestType;
            let is_null = match test.arg.as_ref().map(|a| string_value(Some(a), tg)) {
                Some(Some(Some(_))) => Some(false),
                Some(Some(None)) => Some(true),
                _ => None,
            };
            match (is_null, test.nulltesttype) {
                (Some(v), NullTestType::IsNull) => {
                    if v {
                        Tri::True
                    } else {
                        Tri::False
                    }
                }
                (Some(v), NullTestType::IsNotNull) => {
                    if v {
                        Tri::False
                    } else {
                        Tri::True
                    }
                }
                _ => return None,
            }
        }
        _ => Tri::Unknown,
    };
    Some(value)
}

/// The type names of a cast (`names` as strings).
fn cast_type_names(type_name: &ast::TypeName) -> Vec<&str> {
    type_name
        .names
        .iter()
        .filter_map(|n| match n.as_ref() {
            Some(ast::Node::String(s)) => Some(s.sval.as_str()),
            _ => None,
        })
        .collect()
}

/// Whether a cast in a boolean condition keeps a boolean's value: cast to
/// plain `boolean` (not a domain over it), no array.
fn boolean_cast_is_plain(cast: &ast::TypeCast) -> bool {
    let Some(type_name) = cast.type_name.as_ref() else {
        return false;
    };
    if !type_name.array_bounds.is_empty() {
        return false;
    }
    let names = cast_type_names(type_name);
    matches!(names.as_slice(), [n] | [_, n] if *n == "bool" || *n == "boolean")
}

/// The string a node is known to hold: a plain string literal, a cast of one
/// to a text type, or a known `TG_*` / CASE variable. `Some(None)`: known
/// NULL. Typed values (`'1'::int`, `'now'::timestamptz::text`) are unknown:
/// comparing their text would be wrong.
fn string_value(expression: Option<&ast::Node>, tg: Option<&TgValues>) -> Option<Option<String>> {
    match expression? {
        ast::Node::AConst(c) => {
            if c.isnull {
                return Some(None);
            }
            match c.val.as_ref()? {
                ast::AConstValue::String(s) => Some(Some(s.sval.clone())),
                _ => None,
            }
        }
        ast::Node::CollateClause(c) => string_value(c.arg.as_ref(), tg),
        ast::Node::TypeCast(cast) => {
            let truncate = text_cast_of_literal(cast)?;
            match (string_value(cast.arg.as_ref(), tg)?, truncate) {
                (Some(text), Some(length)) => Some(Some(text.chars().take(length).collect())),
                (value, _) => Some(value),
            }
        }
        ast::Node::ColumnRef(column) => {
            let [field] = column.fields.as_slice() else {
                return None;
            };
            let Some(ast::Node::String(name)) = field.as_ref() else {
                return None;
            };
            // Only a value the firing determines is known: every other
            // column — an unknown TG_* variable, a user variable named
            // `tg_*` — is UNKNOWN, never NULL.
            Some(Some(tg?.get(&name.sval)?.to_owned()))
        }
        _ => None,
    }
}

/// How a cast keeps a plain string literal's text: `None` when it isn't a
/// text cast (`'1'::int`); `Some(None)` when it keeps the text whole
/// (`text`, `name`, `varchar` without a length); `Some(Some(n))` for a
/// `varchar(n)`, which truncates to `n` characters (`'INSERTx'` becomes
/// `INSERT`).
fn text_cast_of_literal(cast: &ast::TypeCast) -> Option<Option<usize>> {
    let type_name = cast.type_name.as_ref()?;
    if !type_name.array_bounds.is_empty() {
        return None;
    }
    let names = cast_type_names(type_name);
    if !matches!(names.as_slice(), [n] | [_, n] if *n == "text" || *n == "varchar" || *n == "name")
    {
        return None;
    }
    if names.last() == Some(&"varchar")
        && let Some(modifier) = type_name.typmods.first()
    {
        let Some(ast::Node::AConst(c)) = modifier.as_ref() else {
            return None;
        };
        let Some(ast::AConstValue::Integer(i)) = c.val.as_ref() else {
            return None;
        };
        return usize::try_from(i.ival).ok().map(Some);
    }
    if !type_name.typmods.is_empty() {
        return None;
    }
    Some(None)
}

/// Evaluate `=`, `<>`, `!=` and `IN` over literals and `TG_*` values.
fn compare_value(expr: &ast::AExpr, tg: Option<&TgValues>) -> Option<Tri> {
    use typedpg_pg_query::ast::AExprKind;
    let operator = expr.name.first().and_then(|n| match n.as_ref()? {
        ast::Node::String(s) => Some(s.sval.as_str()),
        _ => None,
    });
    let negated = matches!(operator, Some("<>" | "!="));
    let left = string_value(expr.lexpr.as_ref(), tg)?;
    let left = match left {
        Some(needle) => needle,
        // A comparison with NULL is NULL: never true.
        None => return Some(Tri::Null),
    };
    match expr.kind {
        AExprKind::AexprOp => {
            if !matches!(operator, Some("=" | "<>" | "!=")) {
                return None;
            }
            let Some(right) = string_value(expr.rexpr.as_ref(), tg)? else {
                return Some(Tri::Null);
            };
            Some(if (left == right) != negated {
                Tri::True
            } else {
                Tri::False
            })
        }
        AExprKind::AexprIn => {
            let Some(ast::Node::List(list)) = expr.rexpr.as_ref() else {
                return None;
            };
            let needle = left;
            let mut matched = false;
            let mut unknown = false;
            let mut null = false;
            for item in &list.items {
                match string_value(item.as_ref(), tg) {
                    Some(Some(value)) if value == needle => matched = true,
                    Some(Some(_)) => {}
                    Some(None) => null = true,
                    None => unknown = true,
                }
            }
            if matched {
                return Some(if negated { Tri::False } else { Tri::True });
            }
            if unknown {
                return Some(Tri::Unknown);
            }
            if null {
                return Some(Tri::Null);
            }
            Some(if negated { Tri::True } else { Tri::False })
        }
        _ => None,
    }
}

/// Whether a trigger's `WHEN` condition certainly folds to `false` (or
/// `NULL`, which isn't true either): `expression_planner` folds it before
/// execution, and the trigger never fires.
fn trigger_when_never_fires(trigger: &Trigger, snapshot: &PgCatalog) -> bool {
    trigger.when.as_ref().is_some_and(|w| {
        matches!(
            crate::expr::plan_fold::certainly(Some(w), snapshot),
            crate::expr::plan_fold::Certain::Bool(false) | crate::expr::plan_fold::Certain::Null
        )
    })
}

/// The requirements a SQL-standard body (`BEGIN ATOMIC ... END` / `RETURN
/// expr`) resolves to at CREATE FUNCTION: PG stores the body analyzed, so
/// the relations, routines and sequences it runs are fixed then, by OID,
/// under the creation `search_path`. Running it checks no schema USAGE —
/// the stored tree looks no name up.
pub(crate) fn resolve_sql_standard_body(
    catalog: &PgCatalog,
    statements: &[typedpg_pg_query::ast::Node],
    proc: &crate::pg_catalog::PgProc,
) -> Requirements {
    // What the analyzer can't read is skipped; the errors the body does
    // raise are the validator's (it analyzes it again), so this one's
    // mustn't leave a captured diagnostic behind.
    let _barrier = crate::error::DiagContextGuard::barrier();
    let params = crate::ddl::function_body::input_params(proc);
    let param_types: Vec<crate::oid::PgTypeOid> = params.iter().map(|(_, t)| *t).collect();
    let (_, mut required) = collect(|| {
        for statement in statements {
            if !is_analyzable(statement) {
                continue;
            }
            let namespace = crate::expr::SqlFunctionParams {
                name: proc.proname.clone(),
                params: params.clone(),
                locals: Vec::new(),
            };
            let _ = crate::expr::with_sql_function_params(namespace, || {
                crate::resolve::without_execution(|| {
                    crate::resolve::analyze_raw_node_with_param_types(
                        catalog,
                        statement,
                        &param_types,
                    )
                })
            });
        }
    });
    // The stored tree doesn't resolve names again.
    required.schemas.clear();
    required
}

/// The statement `query` is, when it is one the analyzer reads.
fn full_statement(query: &str) -> Option<typedpg_pg_query::ast::Node> {
    let parsed = typedpg_pg_query::parse(query).ok()?;
    let [raw] = parsed.tree.stmts.as_slice() else {
        return None;
    };
    let statement = raw.stmt.as_ref()?;
    is_analyzable(statement).then(|| statement.clone())
}

/// A PL/pgSQL statement's SQL text as a statement to analyze: a query as
/// written, or an expression (a condition, a `PERFORM` argument, an
/// assignment's right-hand side) wrapped as `SELECT expression`.
fn statement_or_expression(query: &str) -> Option<typedpg_pg_query::ast::Node> {
    if let Ok(parsed) = typedpg_pg_query::parse(query) {
        // A complete statement the analyzer reads, or a utility statement
        // it doesn't: never an expression.
        let statement = parsed.tree.stmts.first().and_then(|r| r.stmt.as_ref())?;
        return is_analyzable(statement).then(|| statement.clone());
    }
    // PL/pgSQL's assignment expressions are written `target := value`.
    let expression = query
        .split_once(":=")
        .map_or(query, |(_, rhs)| rhs)
        .trim_start();
    let sql = format!("SELECT {expression}");
    full_statement(&sql)
}

/// Whether a routine of `nargs` input parameters (with `defaults` of them
/// defaulted, and `variadic` when its last one is `VARIADIC`) can be called
/// with `written` arguments.
fn call_arity_matches(nargs: usize, defaults: i16, variadic: bool, written: usize) -> bool {
    let defaults = usize::try_from(defaults).unwrap_or(0);
    if variadic {
        return written + 1 >= nargs;
    }
    written == nargs || (written < nargs && written + defaults >= nargs)
}

/// `get_relkind_objtype`'s word for a relation, as `aclcheck_error` says it.
fn relkind_word(relkind: RelKind) -> &'static str {
    match relkind {
        RelKind::View => "view",
        RelKind::MaterializedView => "materialized view",
        RelKind::Sequence => "sequence",
        RelKind::ForeignTable => "foreign table",
        _ => "table",
    }
}

#[cfg(test)]
mod unwind_guards {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    #[test]
    fn collect_restores_its_thread_state_after_a_panic() {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            collect(|| {
                note_schema(PgNamespaceOid::from_raw(1));
                panic!("boom");
            })
        }));
        assert!(COLLECTOR.with(|c| c.borrow().is_none()));
        assert_eq!(SUSPENDED.with(Cell::get), 0);
        assert_eq!(NOT_EXECUTED.with(Cell::get), 0);
        // A later collection works and sees no leftover requirement.
        let ((), req) = collect(|| note_sequence(PgClassOid::from_raw(42), 1));
        assert_eq!(req.sequences, vec![(PgClassOid::from_raw(42), 1)]);
        assert!(req.schemas.is_empty());
    }

    #[test]
    fn scoped_modes_restore_their_state_after_a_panic() {
        let _ = catch_unwind(AssertUnwindSafe(|| suspended(|| panic!("boom"))));
        assert_eq!(SUSPENDED.with(Cell::get), 0);
        let _ = catch_unwind(AssertUnwindSafe(|| not_executed(|| panic!("boom"))));
        assert_eq!(NOT_EXECUTED.with(Cell::get), 0);
        let _ = catch_unwind(AssertUnwindSafe(|| as_role("some_role", || panic!("boom"))));
        assert!(CHECKING.with(|c| c.borrow().is_none()));
        let _ = catch_unwind(AssertUnwindSafe(|| all_code_checked(|| panic!("boom"))));
        assert_eq!(ALL_CODE.with(Cell::get), 0);
        VIEW_COLLECTOR.with(|c| *c.borrow_mut() = Some(Requirements::default()));
        let _ = catch_unwind(AssertUnwindSafe(|| collect_view_qual(|| panic!("boom"))));
        assert!(VIEW_QUALS.with(|c| c.borrow().is_none()));
        VIEW_COLLECTOR.with(|c| *c.borrow_mut() = None);
    }
}
