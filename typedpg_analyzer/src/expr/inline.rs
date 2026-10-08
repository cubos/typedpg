//! A call of a `LANGUAGE sql` function read as its body.
//!
//! A function whose body is one expression (`SELECT expr` / `RETURN expr`,
//! see [`crate::ddl::function_body::inlinable_body`]) computes, for each
//! call, that expression over the call's arguments — what PG's
//! `inline_function` substitutes when it can, and what every call does in
//! any case. Analyzing the body over the arguments, as they are where the
//! call is, says more of the result than its declared type does: that it
//! isn't NULL (`coalesce($1, 0)`), its refinement (`CASE … THEN 'a' …
//! END`), its array elements'.
//!
//! The body is read as it resolves now, as the call does (a string body
//! is parsed at each call; a `RETURN` one was bound at creation, to the
//! objects its names still name unless they were renamed or shadowed).
//! It isn't read for a function with SET items (its own `search_path`),
//! a call leaving parameters to their defaults, a variadic call, nor
//! past a depth or into a function already being read (recursion).

use std::cell::RefCell;

use super::{Ctx, ExprType, TypeGoal, infer_expr};
use crate::oid::PgProcOid;

/// How deep calls are read into bodies of other calls.
const MAX_DEPTH: usize = 8;

thread_local! {
    /// The functions whose bodies are being read.
    static READING: RefCell<Vec<PgProcOid>> = const { RefCell::new(Vec::new()) };
    /// Calls are only resolved, not read as their bodies (see
    /// [`resolving_only`]).
    static RESOLVING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether calls are only being resolved (a DDL statement is running: see
/// [`resolving_only`]).
pub(crate) fn resolving() -> bool {
    RESOLVING.with(std::cell::Cell::get)
}

/// Run `f` with calls only resolved, not read as their bodies: what an
/// expression resolves to (a CHECK constraint's, kept by OID; a DDL
/// statement's validation) doesn't depend on them.
pub(crate) fn resolving_only<R>(f: impl FnOnce() -> R) -> R {
    let outer = RESOLVING.with(|r| r.replace(true));
    let result = f();
    RESOLVING.with(|r| r.set(outer));
    result
}

/// What the body of function `proc` makes of the arguments `args` (in
/// declared order, already of the call), its result converted to the
/// declared return type `ret`; `None` when the call isn't read as its
/// body, or the body doesn't analyze.
pub(crate) fn inline_call(
    proc: &crate::pg_catalog::PgProc,
    args: &[ExprType],
    ret: crate::oid::PgTypeOid,
    ctx: Ctx<'_>,
) -> Option<ExprType> {
    let snapshot = ctx.snapshot;
    let body = snapshot.inline_sql_bodies.get(&proc.oid)?;
    if resolving()
        || snapshot.procs_with_config.contains(&proc.oid)
        || args.len() != proc.proargtypes.len()
        || READING.with(|r| {
            let r = r.borrow();
            r.len() >= MAX_DEPTH || r.contains(&proc.oid)
        })
    {
        return None;
    }
    // Each argument as the body sees it: of the parameter's type (an
    // implicit conversion may NULL it), and — for a STRICT function, which
    // returns NULL without running its body when one is — non-NULL.
    let bound: Vec<ExprType> = args
        .iter()
        .zip(&proc.proargtypes)
        .map(|(a, &declared)| {
            let mut t = a.clone();
            let pseudo = snapshot
                .pg_type
                .get(&declared)
                .is_some_and(|ty| ty.typtype == crate::pg_catalog::TypType::Pseudo);
            if !pseudo && declared != t.type_oid {
                t.note_coerced_to(declared, snapshot);
                t.type_oid = declared;
                t.typmod = None;
            }
            if proc.proisstrict {
                t.nullable = false;
            }
            t
        })
        .collect();
    // Named parameters are columns of a source named after the function
    // (`v`, `f.v`); `$n` are the bound arguments.
    let alias = proc.proname.clone();
    // The input parameters' names (`proargnames` lists OUT ones too, by
    // `proargmodes`).
    let input_names = proc.proargnames.iter().enumerate().filter(|(i, _)| {
        proc.proargmodes.get(*i).is_none_or(|m| {
            matches!(
                m,
                crate::pg_catalog::ArgMode::In
                    | crate::pg_catalog::ArgMode::InOut
                    | crate::pg_catalog::ArgMode::Variadic
            )
        })
    });
    let columns: Vec<crate::scope::ScopeColumn> = input_names
        .map(|(_, name)| name)
        .zip(&bound)
        .filter(|(name, _)| !name.is_empty())
        .map(|(name, t)| crate::scope::ScopeColumn {
            name: name.clone(),
            type_oid: t.type_oid,
            base_not_null: !t.nullable,
            typmod: t.typmod,
            collation: t.collation,
            table_alias: alias.clone(),
            record_fields: t.record_fields.clone(),
            elem_nullable: t.elem_nullable,
            refine: t.refine.clone(),
            origin: None,
        })
        .collect();
    let mut scope = crate::scope::Scope::default();
    scope
        .add_derived(&alias, columns, crate::scope::SourceKind::Other)
        .ok()?;
    let null_ctx = crate::nullability::NullabilityContext::default();
    let mut params = crate::param_collector::ParamCollector::bound(bound);
    READING.with(|r| r.borrow_mut().push(proc.oid));
    let (result, _) = crate::ddl::depend::collect(|| {
        let _level = crate::resolve::QueryLevel::enter();
        infer_expr(
            body,
            Ctx::new(&scope, &null_ctx, snapshot),
            &mut params,
            TypeGoal::NONE,
        )
    });
    READING.with(|r| r.borrow_mut().pop());
    let mut t = result.ok()?;
    // The result is converted to the declared return type.
    let same = snapshot.unwrap_domain(t.type_oid) == snapshot.unwrap_domain(ret);
    t.note_coerced_to(ret, snapshot);
    if !same {
        t.elem_nullable = None;
        t.record_fields = None;
    }
    t.type_oid = ret;
    if proc.proisstrict && args.iter().any(|a| a.nullable) {
        t.nullable = true;
    }
    Some(t)
}
