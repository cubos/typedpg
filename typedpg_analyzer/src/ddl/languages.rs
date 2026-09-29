//! Procedural languages (`pg_language`): functions, DO blocks and
//! transforms name one that must exist; CREATE LANGUAGE ... HANDLER adds
//! one, and the built-in ones can't be dropped.

use typedpg_pg_query::protobuf::{CreatePLangStmt, CreateTransformStmt};

use super::DdlError;
use crate::oid::PgTypeOid;
use crate::pg_catalog::PgCatalog;

/// The languages of a stock database.
pub(crate) const BUILTIN: &[&str] = &["internal", "c", "sql", "plpgsql"];

pub(crate) fn exists(interp: &PgCatalog, name: &str) -> bool {
    BUILTIN.contains(&name) && !interp.dropped_languages.iter().any(|l| l == name)
        || interp.languages.iter().any(|l| l == name)
}

/// get_language_oid.
pub(crate) fn check(interp: &PgCatalog, name: &str) -> Result<(), DdlError> {
    if exists(interp, name) {
        return Ok(());
    }
    Err(DdlError::TypeNotFound(format!(
        "language \"{name}\" does not exist (Use CREATE EXTENSION to load the language into the \
         database.)"
    )))
}

/// CREATE [OR REPLACE] LANGUAGE name HANDLER ... (CreateProceduralLanguage).
pub fn create_language(interp: &mut PgCatalog, stmt: &CreatePLangStmt) -> Result<(), DdlError> {
    if exists(interp, &stmt.plname) {
        if stmt.replace {
            return Ok(());
        }
        return Err(DdlError::DuplicateObject(format!(
            "language \"{}\" already exists",
            stmt.plname
        )));
    }
    let parts: Vec<&str> = stmt
        .plhandler
        .iter()
        .filter_map(super::util::node_string)
        .collect();
    if let [.., handler] = parts.as_slice() {
        let schema = (parts.len() == 2).then(|| parts[0]);
        let found = interp
            .find_functions(schema, handler)
            .into_iter()
            .any(|p| p.proargtypes.is_empty());
        if !found {
            return Err(DdlError::TypeNotFound(format!(
                "function {handler}() does not exist"
            )));
        }
    }
    interp.languages.push(stmt.plname.clone());
    Ok(())
}

/// DROP LANGUAGE [IF EXISTS] name.
pub(crate) fn drop_language(
    interp: &mut PgCatalog,
    obj_node: &typedpg_pg_query::protobuf::Node,
    missing_ok: bool,
) -> Result<(), DdlError> {
    let Some(name) = super::util::node_string(obj_node).map(str::to_owned) else {
        return Ok(());
    };
    if !exists(interp, &name) {
        if missing_ok {
            return Ok(());
        }
        return Err(check(interp, &name).unwrap_err());
    }
    match name.as_str() {
        "internal" | "c" | "sql" => {
            return Err(DdlError::DependencyError(format!(
                "cannot drop language {name} because it is required by the database system"
            )));
        }
        "plpgsql" => {
            return Err(DdlError::DependencyError(
                "cannot drop language plpgsql because extension plpgsql requires it (You can \
                 drop extension plpgsql instead.)"
                    .into(),
            ));
        }
        _ => {}
    }
    interp.languages.retain(|l| *l != name);
    if BUILTIN.contains(&name.as_str()) {
        interp.dropped_languages.push(name);
    }
    Ok(())
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
    check(interp, &old)?;
    if exists(interp, &stmt.newname) {
        return Err(DdlError::DuplicateObject(format!(
            "language \"{}\" already exists",
            stmt.newname
        )));
    }
    if let Some(l) = interp.languages.iter_mut().find(|l| **l == old) {
        l.clone_from(&stmt.newname);
    } else {
        interp.dropped_languages.push(old);
        interp.languages.push(stmt.newname.clone());
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
