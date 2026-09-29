//! Letting libpg_query's catalog lookups read the caller's catalog.
//!
//! PL/pgSQL's compiler resolves types (of arguments, the result and declared
//! variables), schemas and, for `%TYPE` / `%ROWTYPE`, relations and columns.
//! libpg_query has no server behind it, so by default those lookups hit mocks
//! that only know the built-in types. [`parse_plpgsql_with_catalog`] installs
//! a [`Catalog`] for the duration of the call instead, and the compiler then
//! resolves everything — with PostgreSQL's own errors — against it.

use std::any::Any;
use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

use crate::ffi;

/// A `pg_type` row, with the columns the parser and the PL/pgSQL compiler
/// read. OIDs of `0` stand for none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogType {
    pub oid: u32,
    pub name: String,
    pub namespace: u32,
    /// `typlen`: a fixed size, `-1` for varlena, `-2` for C strings.
    pub len: i16,
    pub by_val: bool,
    /// `typtype`: `b`, `c`, `d`, `e`, `p`, `r` or `m`.
    pub typtype: u8,
    pub category: u8,
    pub preferred: bool,
    /// `typalign`: `c`, `s`, `i` or `d`.
    pub align: u8,
    pub relid: u32,
    /// `typsubscript`: the subscripting handler function.
    pub subscript: u32,
    pub elem: u32,
    pub array: u32,
    pub base_type: u32,
    pub typmod: i32,
    pub not_null: bool,
    pub collation: u32,
    /// `typisdefined`: `false` for a shell type.
    pub is_defined: bool,
}

/// A `pg_attribute` row, with the columns `%TYPE` reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogAttribute {
    pub number: i16,
    pub type_oid: u32,
    pub typmod: i32,
    pub collation: u32,
}

/// A PostgreSQL catalog, as the PL/pgSQL compiler consults it.
pub trait Catalog {
    fn type_by_oid(&self, oid: u32) -> Option<CatalogType>;
    fn type_by_name(&self, namespace: u32, name: &str) -> Option<u32>;
    fn namespace_by_name(&self, name: &str) -> Option<u32>;
    fn namespace_name(&self, namespace: u32) -> Option<String>;
    /// The schemas an unqualified name is looked up in, in order — the
    /// effective search path, `pg_catalog` included where it applies.
    fn search_path(&self) -> Vec<u32>;
    fn relation_by_name(&self, namespace: u32, name: &str) -> Option<u32>;
    /// `pg_class.reltype` (`None` for a relkind without a row type).
    fn relation_type(&self, relation: u32) -> Option<u32>;
    fn attribute_by_name(&self, relation: u32, name: &str) -> Option<CatalogAttribute>;
    fn attribute_by_number(&self, relation: u32, number: i16) -> Option<CatalogAttribute>;
}

/// The state one installed catalog carries across callbacks.
struct Session<'a> {
    catalog: &'a dyn Catalog,
    /// Strings handed to C, kept alive until the call returns.
    strings: RefCell<Vec<CString>>,
    /// A panic raised by the catalog, re-raised once C has returned.
    panic: RefCell<Option<Box<dyn Any + Send>>>,
}

impl Session<'_> {
    fn keep(&self, s: &str) -> *const c_char {
        let owned = CString::new(s.replace('\0', "")).expect("NULs removed");
        let ptr = owned.as_ptr();
        self.strings.borrow_mut().push(owned);
        ptr
    }
}

/// Run `f` on the session behind `ctx`, never letting a panic unwind into C:
/// it is stored and `default` returned.
fn with_session<R>(ctx: *mut c_void, default: R, f: impl FnOnce(&Session<'_>) -> R) -> R {
    // SAFETY: `ctx` is the `Session` installed by `with_catalog`, alive for
    // the whole C call these callbacks run in.
    let session = unsafe { &*(ctx as *const Session<'_>) };
    if session.panic.borrow().is_some() {
        return default;
    }
    match catch_unwind(AssertUnwindSafe(|| f(session))) {
        Ok(r) => r,
        Err(payload) => {
            *session.panic.borrow_mut() = Some(payload);
            default
        }
    }
}

/// # Safety
/// `s` must be a valid NUL-terminated string.
unsafe fn str_arg<'a>(s: *const c_char) -> std::borrow::Cow<'a, str> {
    unsafe { CStr::from_ptr(s) }.to_string_lossy()
}

extern "C" fn type_by_oid(ctx: *mut c_void, oid: u32, out: *mut ffi::TypedpgType) -> bool {
    with_session(ctx, false, |s| {
        let Some(t) = s.catalog.type_by_oid(oid) else {
            return false;
        };
        let row = ffi::TypedpgType {
            oid: t.oid,
            typname: s.keep(&t.name),
            typnamespace: t.namespace,
            typlen: t.len,
            typbyval: t.by_val,
            typtype: t.typtype as c_char,
            typcategory: t.category as c_char,
            typispreferred: t.preferred,
            typalign: t.align as c_char,
            typrelid: t.relid,
            typsubscript: t.subscript,
            typelem: t.elem,
            typarray: t.array,
            typbasetype: t.base_type,
            typtypmod: t.typmod,
            typnotnull: t.not_null,
            typcollation: t.collation,
            typisdefined: t.is_defined,
        };
        // SAFETY: C passes a writable TypedpgType.
        unsafe { out.write(row) };
        true
    })
}

extern "C" fn type_by_name(ctx: *mut c_void, namespace: u32, name: *const c_char) -> u32 {
    with_session(ctx, 0, |s| {
        s.catalog
            .type_by_name(namespace, &unsafe { str_arg(name) })
            .unwrap_or(0)
    })
}

extern "C" fn namespace_by_name(ctx: *mut c_void, name: *const c_char) -> u32 {
    with_session(ctx, 0, |s| {
        s.catalog
            .namespace_by_name(&unsafe { str_arg(name) })
            .unwrap_or(0)
    })
}

extern "C" fn namespace_name(ctx: *mut c_void, namespace: u32) -> *const c_char {
    with_session(ctx, std::ptr::null(), |s| {
        s.catalog
            .namespace_name(namespace)
            .map_or(std::ptr::null(), |n| s.keep(&n))
    })
}

extern "C" fn search_path(ctx: *mut c_void, out: *mut u32, capacity: usize) -> usize {
    with_session(ctx, 0, |s| {
        let path = s.catalog.search_path();
        for (i, &oid) in path.iter().take(capacity).enumerate() {
            // SAFETY: C passes room for `capacity` OIDs.
            unsafe { out.add(i).write(oid) };
        }
        path.len()
    })
}

extern "C" fn relation_by_name(ctx: *mut c_void, namespace: u32, name: *const c_char) -> u32 {
    with_session(ctx, 0, |s| {
        s.catalog
            .relation_by_name(namespace, &unsafe { str_arg(name) })
            .unwrap_or(0)
    })
}

extern "C" fn relation_type(ctx: *mut c_void, relation: u32) -> u32 {
    with_session(ctx, 0, |s| s.catalog.relation_type(relation).unwrap_or(0))
}

fn write_attribute(out: *mut ffi::TypedpgAttribute, a: Option<CatalogAttribute>) -> bool {
    let Some(a) = a else {
        return false;
    };
    // SAFETY: C passes a writable TypedpgAttribute.
    unsafe {
        out.write(ffi::TypedpgAttribute {
            attnum: a.number,
            atttypid: a.type_oid,
            atttypmod: a.typmod,
            attcollation: a.collation,
        })
    };
    true
}

extern "C" fn attribute_by_name(
    ctx: *mut c_void,
    relation: u32,
    name: *const c_char,
    out: *mut ffi::TypedpgAttribute,
) -> bool {
    with_session(ctx, false, |s| {
        write_attribute(
            out,
            s.catalog
                .attribute_by_name(relation, &unsafe { str_arg(name) }),
        )
    })
}

extern "C" fn attribute_by_number(
    ctx: *mut c_void,
    relation: u32,
    number: i16,
    out: *mut ffi::TypedpgAttribute,
) -> bool {
    with_session(ctx, false, |s| {
        write_attribute(out, s.catalog.attribute_by_number(relation, number))
    })
}

/// Run `f` (a libpg_query call) with `catalog` installed on this thread.
pub(crate) fn with_catalog<R>(catalog: &dyn Catalog, f: impl FnOnce() -> R) -> R {
    let session = Session {
        catalog,
        strings: RefCell::new(Vec::new()),
        panic: RefCell::new(None),
    };
    let hooks = ffi::TypedpgCatalog {
        ctx: &session as *const Session<'_> as *mut c_void,
        type_by_oid,
        type_by_name,
        namespace_by_name,
        namespace_name,
        search_path,
        relation_by_name,
        relation_type,
        attribute_by_name,
        attribute_by_number,
    };
    /// Uninstalls the hooks however `f` exits.
    struct Installed;
    impl Drop for Installed {
        fn drop(&mut self) {
            // SAFETY: clearing the thread's catalog pointer.
            unsafe { ffi::typedpg_catalog_install(std::ptr::null()) };
        }
    }
    // SAFETY: `hooks` and `session` outlive the installation, which `_guard`
    // ends before they are dropped.
    unsafe { ffi::typedpg_catalog_install(&hooks) };
    let result = {
        let _guard = Installed;
        f()
    };
    if let Some(payload) = session.panic.borrow_mut().take() {
        resume_unwind(payload);
    }
    result
}
