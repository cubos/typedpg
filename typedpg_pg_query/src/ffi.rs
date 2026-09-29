//! The part of libpg_query's C API (`pg_query.h`) the crate calls.

use std::os::raw::{c_char, c_int};

#[repr(C)]
pub(crate) struct PgQueryError {
    pub message: *mut c_char,
    pub funcname: *mut c_char,
    pub filename: *mut c_char,
    pub lineno: c_int,
    pub cursorpos: c_int,
    pub context: *mut c_char,
}

#[repr(C)]
pub(crate) struct PgQueryProtobuf {
    pub len: usize,
    pub data: *mut c_char,
}

#[repr(C)]
pub(crate) struct PgQueryScanResult {
    pub pbuf: PgQueryProtobuf,
    pub stderr_buffer: *mut c_char,
    pub error: *mut PgQueryError,
}

#[repr(C)]
pub(crate) struct PgQueryProtobufParseResult {
    pub parse_tree: PgQueryProtobuf,
    pub stderr_buffer: *mut c_char,
    pub error: *mut PgQueryError,
}

#[repr(C)]
pub(crate) struct PgQueryDeparseResult {
    pub query: *mut c_char,
    pub error: *mut PgQueryError,
}

#[repr(C)]
pub(crate) struct PgQueryPlpgsqlParseResult {
    pub plpgsql_funcs: *mut c_char,
    pub error: *mut PgQueryError,
}

unsafe extern "C" {
    pub(crate) fn pg_query_parse_protobuf(input: *const c_char) -> PgQueryProtobufParseResult;
    pub(crate) fn pg_query_free_protobuf_parse_result(result: PgQueryProtobufParseResult);
    pub(crate) fn pg_query_scan(input: *const c_char) -> PgQueryScanResult;
    pub(crate) fn pg_query_free_scan_result(result: PgQueryScanResult);
    pub(crate) fn pg_query_deparse_protobuf(parse_tree: PgQueryProtobuf) -> PgQueryDeparseResult;
    pub(crate) fn pg_query_free_deparse_result(result: PgQueryDeparseResult);
    pub(crate) fn pg_query_parse_plpgsql(input: *const c_char) -> PgQueryPlpgsqlParseResult;
    pub(crate) fn pg_query_free_plpgsql_parse_result(result: PgQueryPlpgsqlParseResult);
}

/// `TypedpgType` in csrc/catalog.h.
#[repr(C)]
pub(crate) struct TypedpgType {
    pub oid: u32,
    pub typname: *const c_char,
    pub typnamespace: u32,
    pub typlen: i16,
    pub typbyval: bool,
    pub typtype: c_char,
    pub typcategory: c_char,
    pub typispreferred: bool,
    pub typalign: c_char,
    pub typrelid: u32,
    pub typsubscript: u32,
    pub typelem: u32,
    pub typarray: u32,
    pub typbasetype: u32,
    pub typtypmod: i32,
    pub typnotnull: bool,
    pub typcollation: u32,
}

/// `TypedpgAttribute` in csrc/catalog.h.
#[repr(C)]
pub(crate) struct TypedpgAttribute {
    pub attnum: i16,
    pub atttypid: u32,
    pub atttypmod: i32,
    pub attcollation: u32,
}

/// `TypedpgCatalog` in csrc/catalog.h.
#[repr(C)]
pub(crate) struct TypedpgCatalog {
    pub ctx: *mut std::ffi::c_void,
    pub type_by_oid: extern "C" fn(*mut std::ffi::c_void, u32, *mut TypedpgType) -> bool,
    pub type_by_name: extern "C" fn(*mut std::ffi::c_void, u32, *const c_char) -> u32,
    pub namespace_by_name: extern "C" fn(*mut std::ffi::c_void, *const c_char) -> u32,
    pub namespace_name: extern "C" fn(*mut std::ffi::c_void, u32) -> *const c_char,
    pub search_path: extern "C" fn(*mut std::ffi::c_void, *mut u32, usize) -> usize,
    pub relation_by_name: extern "C" fn(*mut std::ffi::c_void, u32, *const c_char) -> u32,
    pub relation_type: extern "C" fn(*mut std::ffi::c_void, u32) -> u32,
    pub attribute_by_name:
        extern "C" fn(*mut std::ffi::c_void, u32, *const c_char, *mut TypedpgAttribute) -> bool,
    pub attribute_by_number:
        extern "C" fn(*mut std::ffi::c_void, u32, i16, *mut TypedpgAttribute) -> bool,
}

unsafe extern "C" {
    pub(crate) fn typedpg_catalog_install(catalog: *const TypedpgCatalog);
}
