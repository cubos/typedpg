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
