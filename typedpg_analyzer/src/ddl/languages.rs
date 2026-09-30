//! Procedural languages (`pg_language`): functions, DO blocks and
//! transforms name one that must exist; CREATE LANGUAGE ... HANDLER adds
//! one (depending on its handler functions), a routine depends on its
//! language, and DROP LANGUAGE goes through the dependency engine — the
//! built-in languages are pinned, plpgsql belongs to its extension.

use typedpg_pg_query::protobuf::{CreatePLangStmt, CreateTransformStmt};

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
    names: &[typedpg_pg_query::protobuf::Node],
    args: &[PgTypeOid],
) -> Result<Option<PgProcOid>, DdlError> {
    let parts: Vec<String> = names
        .iter()
        .filter_map(super::util::node_string)
        .map(str::to_owned)
        .collect();
    if parts.is_empty() {
        return Ok(None);
    }
    match super::functions::lookup_func_name(interp, &parts, Some(args))? {
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
    obj_node: &typedpg_pg_query::protobuf::Node,
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
    stmt: &typedpg_pg_query::protobuf::RenameStmt,
) -> Result<(), DdlError> {
    let Some(old) = stmt
        .object
        .as_deref()
        .and_then(super::util::node_string)
        .map(str::to_owned)
    else {
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
pub fn create_transform(
    interp: &mut PgCatalog,
    stmt: &CreateTransformStmt,
) -> Result<(), DdlError> {
    let Some(tn) = stmt.type_name.as_ref() else {
        return Ok(());
    };
    let typ = super::util::lookup_type_name(tn, interp)?;
    check(interp, &stmt.lang)?;
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
    object: &typedpg_pg_query::protobuf::Node,
) -> Result<Option<usize>, DdlError> {
    use typedpg_pg_query::protobuf::node;
    let Some(node::Node::List(l)) = object.node.as_ref() else {
        return Ok(None);
    };
    let [typ, lang] = l.items.as_slice() else {
        return Ok(None);
    };
    let (Some(node::Node::TypeName(tn)), Some(lang)) =
        (typ.node.as_ref(), super::util::node_string(lang))
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
    object: &typedpg_pg_query::protobuf::Node,
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
