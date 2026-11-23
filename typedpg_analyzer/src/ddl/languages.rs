//! Procedural languages (`pg_language`): functions, DO blocks and
//! transforms name one that must exist; CREATE LANGUAGE ... HANDLER adds
//! one (depending on its handler functions), a routine depends on its
//! language, and DROP LANGUAGE goes through the dependency engine — the
//! built-in languages are pinned, plpgsql belongs to its extension.

use typedpg_pg_query::ast::{CreatePLangStmt, CreateTransformStmt};

use super::DdlError;
use crate::oid::{PgLanguageOid, PgProcOid, PgTypeOid};
use crate::pg_catalog::{PgCatalog, PgLanguage};

/// `FirstUnpinnedObjectId`: the built-in languages below it (internal, c,
/// sql) are pinned — required by the database system.
const FIRST_UNPINNED_OBJECT_ID: u32 = 12000;

/// get_language_oid(missing_ok = true).
pub(crate) fn find(interp: &PgCatalog, name: &str) -> Option<PgLanguageOid> {
    interp
        .pg_language
        .values()
        .find(|l| l.lanname == name)
        .map(|l| l.oid)
}

pub(crate) fn exists(interp: &PgCatalog, name: &str) -> bool {
    find(interp, name).is_some()
}

fn not_found(name: &str) -> DdlError {
    DdlError::TypeNotFound(format!(
        "language \"{name}\" does not exist (Use CREATE EXTENSION to load the language into the \
         database.)"
    ))
}

/// get_language_oid.
pub(crate) fn check(interp: &PgCatalog, name: &str) -> Result<PgLanguageOid, DdlError> {
    find(interp, name).ok_or_else(|| not_found(name))
}

/// The `name(args)` function of a CREATE LANGUAGE clause, if written.
fn clause_function(
    interp: &PgCatalog,
    names: &[Option<typedpg_pg_query::ast::Node>],
    args: &[PgTypeOid],
) -> Result<Option<PgProcOid>, DdlError> {
    let parts: Vec<String> = names
        .iter()
        .filter_map(|n| super::util::node_string(n.as_ref()))
        .map(str::to_owned)
        .collect();
    if parts.is_empty() {
        return Ok(None);
    }
    match super::functions::lookup_func_name(interp, &parts, Some(args), false)? {
        Some(oid) => Ok(Some(oid)),
        None => Err(DdlError::TypeNotFound(format!(
            "function {} does not exist",
            super::functions::func_signature_string(interp, &parts, args)
        ))),
    }
}

/// CREATE [OR REPLACE] [TRUSTED] LANGUAGE name HANDLER h [INLINE i]
/// [VALIDATOR v] (CreateProceduralLanguage): the language depends on its
/// handler, inline and validator functions.
pub fn create_language(interp: &mut PgCatalog, stmt: &CreatePLangStmt) -> Result<(), DdlError> {
    let existing = find(interp, &stmt.plname);
    if existing.is_some() && !stmt.replace {
        return Err(DdlError::DuplicateObject(format!(
            "language \"{}\" already exists",
            stmt.plname
        )));
    }
    const INTERNAL: PgTypeOid = PgTypeOid::from_raw(2281);
    const OID: PgTypeOid = PgTypeOid::from_raw(26);
    let handler = clause_function(interp, &stmt.plhandler, &[])?;
    let inline = clause_function(interp, &stmt.plinline, &[INTERNAL])?;
    let validator = clause_function(interp, &stmt.plvalidator, &[OID])?;
    let oid = match existing {
        Some(oid) => oid,
        None => PgLanguageOid::from_nonzero(interp.alloc_oid()?),
    };
    let created = existing.is_none();
    interp.pg_language.insert(
        oid,
        PgLanguage {
            oid,
            lanname: stmt.plname.clone(),
            lanispl: true,
            lanpltrusted: stmt.pltrusted,
            lanplcallfoid: handler,
        },
    );
    if created {
        let owner = super::roles::current_role(interp);
        super::acl::language_created(interp, oid, owner);
    }
    let addr = super::depend::ObjectAddress::language(oid);
    super::depend::forget_dependencies_of(interp, addr);
    super::depend::record(
        interp,
        addr,
        [handler, inline, validator]
            .into_iter()
            .flatten()
            .map(super::depend::ObjectAddress::proc),
        crate::pg_catalog::DepType::Normal,
    );
    Ok(())
}

/// DROP LANGUAGE [IF EXISTS] name [CASCADE]: the built-in languages are
/// pinned, an extension's language goes with its extension, and the
/// routines written in it need CASCADE.
pub(crate) fn drop_language(
    interp: &mut PgCatalog,
    obj_node: Option<&typedpg_pg_query::ast::Node>,
    missing_ok: bool,
    cascade: bool,
) -> Result<(), DdlError> {
    let Some(name) = super::util::node_string(obj_node).map(str::to_owned) else {
        return Ok(());
    };
    let Some(oid) = find(interp, &name) else {
        if missing_ok {
            return Ok(());
        }
        return Err(not_found(&name));
    };
    if oid.get() < FIRST_UNPINNED_OBJECT_ID {
        return Err(DdlError::DependencyError(format!(
            "cannot drop language {name} because it is required by the database system"
        )));
    }
    let addr = super::depend::ObjectAddress::language(oid);
    let desc = format!("language {name}");
    super::depend::check_not_owned(interp, addr, &desc)?;
    super::depend::drop_dependents(interp, addr, &desc, cascade)?;
    delete_language(interp, oid);
    Ok(())
}

/// Remove language `oid` and its dependency rows.
pub(crate) fn delete_language(interp: &mut PgCatalog, oid: PgLanguageOid) {
    interp.pg_language.remove(&oid);
    let addr = super::depend::ObjectAddress::language(oid);
    interp.remove_dependencies_of(addr.classid, addr.objid);
    interp.remove_dependencies_on(addr.classid, addr.objid);
}

/// ALTER LANGUAGE name RENAME TO new.
pub(crate) fn rename_language(
    interp: &mut PgCatalog,
    stmt: &typedpg_pg_query::ast::RenameStmt,
) -> Result<(), DdlError> {
    let Some(old) = super::util::node_string(stmt.object.as_ref()).map(str::to_owned) else {
        return Ok(());
    };
    let oid = check(interp, &old)?;
    if exists(interp, &stmt.newname) {
        return Err(DdlError::DuplicateObject(format!(
            "language \"{}\" already exists",
            stmt.newname
        )));
    }
    if let Some(l) = interp.pg_language.get_mut(&oid) {
        l.lanname.clone_from(&stmt.newname);
    }
    Ok(())
}

/// CREATE [OR REPLACE] TRANSFORM FOR type LANGUAGE lang (...)
/// (CreateTransform): the type and language must exist, and a transform
/// for the pair is recorded.
/// `internal`, the type of a transform function's argument.
const TRANSFORM_INTERNAL: crate::oid::PgTypeOid = crate::oid::PgTypeOid::from_raw(2281);

pub fn create_transform(
    interp: &mut PgCatalog,
    stmt: &CreateTransformStmt,
) -> Result<(), DdlError> {
    let Some(tn) = stmt.type_name.as_ref() else {
        return Ok(());
    };
    let typ = super::util::lookup_type_name(tn, interp)?;
    // CreateTransform: a real data type, an existing language, then each
    // function: FROM SQL returns internal, TO SQL the data type, and both
    // are plain, non-volatile, non-set functions of one internal argument.
    match interp.pg_type.get(&typ).map(|t| t.typtype) {
        Some(crate::pg_catalog::TypType::Pseudo) => {
            return Err(DdlError::Parse(format!(
                "data type {} is a pseudo-type",
                super::util::format_type_for_message(interp, typ)
            )));
        }
        Some(crate::pg_catalog::TypType::Domain) => {
            return Err(DdlError::Parse(format!(
                "data type {} is a domain",
                super::util::format_type_for_message(interp, typ)
            )));
        }
        _ => {}
    }
    check(interp, &stmt.lang)?;
    for (function, to_sql) in [
        (stmt.fromsql.as_deref(), false),
        (stmt.tosql.as_deref(), true),
    ] {
        let Some(function) = function else {
            continue;
        };
        let Some(oid) = super::functions::lookup_func_with_args(
            interp,
            typedpg_pg_query::ast::ObjectType::ObjectFunction,
            function,
            false,
        )?
        else {
            continue;
        };
        let Some(proc) = interp.pg_proc.get(&oid) else {
            continue;
        };
        let invalid = |message: &str| Err(DdlError::Parse(message.to_owned()));
        if !to_sql && proc.prorettype != TRANSFORM_INTERNAL {
            return invalid("return data type of FROM SQL function must be internal");
        }
        if to_sql && proc.prorettype != typ {
            return invalid("return data type of TO SQL function must be the transform data type");
        }
        // check_transform_function.
        if proc.provolatile == crate::pg_catalog::ProVolatile::Volatile {
            return invalid("transform function must not be volatile");
        }
        if proc.prokind != crate::pg_catalog::ProKind::Function {
            return invalid("transform function must be a normal function");
        }
        if proc.proretset {
            return invalid("transform function must not return a set");
        }
        if proc.proargtypes.len() != 1 {
            return invalid("transform function must take one argument");
        }
        if proc.proargtypes[0] != TRANSFORM_INTERNAL {
            return invalid("first argument of transform function must be type internal");
        }
    }
    let exists = transform_exists(interp, typ, &stmt.lang);
    if exists && !stmt.replace {
        return Err(DdlError::DuplicateObject(format!(
            "transform for type {} language \"{}\" already exists",
            super::util::format_type_for_message(interp, typ),
            stmt.lang
        )));
    }
    if !exists {
        interp.transforms.push((typ, stmt.lang.clone()));
    }
    Ok(())
}

/// Whether a transform for `typeid` and `language` exists (get_transform_oid).
pub(crate) fn transform_exists(interp: &PgCatalog, typeid: PgTypeOid, language: &str) -> bool {
    interp
        .transforms
        .iter()
        .any(|(t, l)| *t == typeid && l == language)
}

/// get_transform_oid: the object of `DROP / COMMENT ON TRANSFORM FOR type
/// LANGUAGE lang` (a `[TypeName, lang]` list). Returns where it is.
pub(crate) fn find_transform(
    interp: &PgCatalog,
    object: Option<&typedpg_pg_query::ast::Node>,
) -> Result<Option<usize>, DdlError> {
    use typedpg_pg_query::ast;
    let Some(ast::Node::List(l)) = object else {
        return Ok(None);
    };
    let [typ, lang] = l.items.as_slice() else {
        return Ok(None);
    };
    let (Some(ast::Node::TypeName(tn)), Some(lang)) =
        (typ.as_ref(), super::util::node_string(lang.as_ref()))
    else {
        return Ok(None);
    };
    let typ = super::util::lookup_type_name(tn, interp)?;
    check(interp, lang)?;
    match interp
        .transforms
        .iter()
        .position(|(t, l)| *t == typ && l == lang)
    {
        Some(at) => Ok(Some(at)),
        None => Err(DdlError::TypeNotFound(format!(
            "transform for type {} language \"{lang}\" does not exist",
            super::util::format_type_for_message(interp, typ)
        ))),
    }
}

/// DROP TRANSFORM [IF EXISTS] FOR type LANGUAGE lang.
pub(crate) fn drop_transform(
    interp: &mut PgCatalog,
    object: Option<&typedpg_pg_query::ast::Node>,
    missing_ok: bool,
) -> Result<(), DdlError> {
    match find_transform(interp, object) {
        Ok(Some(at)) => {
            interp.transforms.remove(at);
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(_) if missing_ok => Ok(()),
        Err(e) => Err(e),
    }
}
