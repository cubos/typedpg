//! PostgreSQL parser bindings for typedpg, built on
//! [libpg_query](https://github.com/pganalyze/libpg_query): the server's own
//! grammar, extracted into a library, with the parse tree handed over as
//! protobuf.
//!
//! - [`parse`] / [`scan`] / [`deparse`] / [`parse_plpgsql`] call into the
//!   C library;
//! - [`protobuf`] holds the AST types, generated from libpg_query's
//!   `pg_query.proto`;
//! - [`NodeRef`] / [`NodeMut`] view a node by kind, and `nodes()` /
//!   `nodes_mut()` list a tree's nodes.
//!
//! The PostgreSQL release the grammar comes from is [`PG_VERSION`]. The
//! generated modules are produced by `typedpg_pg_query_codegen`; see the
//! crate README for moving to a new release.

mod ffi;
mod node;
#[allow(clippy::all, missing_docs)]
pub mod protobuf;

use std::collections::VecDeque;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;

use prost::Message;

pub use node::{NodeMut, NodeRef};
/// The `oneof` of every node kind — what [`protobuf::Node::node`] holds.
pub use protobuf::node::Node as NodeEnum;

include!(concat!(env!("OUT_DIR"), "/version.rs"));

/// Errors from the parser and from moving trees across the C boundary.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    /// The input contains a NUL byte, which C strings can't carry.
    #[error("Invalid statement format: {0}")]
    Conversion(#[from] std::ffi::NulError),
    /// The protobuf the library returned didn't decode.
    #[error("Error decoding result: {0}")]
    Decode(#[from] prost::DecodeError),
    /// PostgreSQL's parser (or deparser) rejected the input; the message is
    /// the server's.
    #[error("Invalid statement: {0}")]
    Parse(String),
    /// The PL/pgSQL parse tree wasn't valid JSON.
    #[error("Error parsing JSON: {0}")]
    InvalidJson(String),
    /// The scanner rejected the input.
    #[error("Error scanning: {0}")]
    Scan(String),
}

/// `Result` with this crate's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// A parsed statement list.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseResult {
    /// The parse tree.
    pub protobuf: protobuf::ParseResult,
    /// Whatever the parser wrote to stderr (warnings).
    pub stderr: String,
}

impl ParseResult {
    /// The SQL this tree deparses to.
    pub fn deparse(&self) -> Result<String> {
        deparse(&self.protobuf)
    }
}

/// The message of a libpg_query error.
///
/// # Safety
/// `error` must be a live `PgQueryError`.
unsafe fn error_message(error: *const ffi::PgQueryError) -> String {
    unsafe { CStr::from_ptr((*error).message) }
        .to_string_lossy()
        .into_owned()
}

/// Parse `sql` with PostgreSQL's grammar.
pub fn parse(sql: &str) -> Result<ParseResult> {
    let input = CString::new(sql)?;
    let result = unsafe { ffi::pg_query_parse_protobuf(input.as_ptr()) };
    let parsed = if result.error.is_null() {
        let data = unsafe {
            std::slice::from_raw_parts(result.parse_tree.data as *const u8, result.parse_tree.len)
        };
        let stderr = if result.stderr_buffer.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(result.stderr_buffer) }
                .to_string_lossy()
                .into_owned()
        };
        protobuf::ParseResult::decode(data)
            .map(|protobuf| ParseResult { protobuf, stderr })
            .map_err(Error::Decode)
    } else {
        Err(Error::Parse(unsafe { error_message(result.error) }))
    };
    unsafe { ffi::pg_query_free_protobuf_parse_result(result) };
    parsed
}

/// Split `sql` into PostgreSQL's lexical tokens.
pub fn scan(sql: &str) -> Result<protobuf::ScanResult> {
    let input = CString::new(sql)?;
    let result = unsafe { ffi::pg_query_scan(input.as_ptr()) };
    let scanned = if result.error.is_null() {
        let data =
            unsafe { std::slice::from_raw_parts(result.pbuf.data as *const u8, result.pbuf.len) };
        protobuf::ScanResult::decode(data).map_err(Error::Decode)
    } else {
        Err(Error::Scan(unsafe { error_message(result.error) }))
    };
    unsafe { ffi::pg_query_free_scan_result(result) };
    scanned
}

/// Turn a parse tree back into SQL.
pub fn deparse(tree: &protobuf::ParseResult) -> Result<String> {
    let buffer = tree.encode_to_vec();
    let input = ffi::PgQueryProtobuf {
        len: buffer.len(),
        // The deparser only reads the buffer.
        data: buffer.as_ptr() as *mut c_char,
    };
    let result = unsafe { ffi::pg_query_deparse_protobuf(input) };
    let sql = if result.error.is_null() {
        Ok(unsafe { CStr::from_ptr(result.query) }
            .to_string_lossy()
            .into_owned())
    } else {
        Err(Error::Parse(unsafe { error_message(result.error) }))
    };
    unsafe { ffi::pg_query_free_deparse_result(result) };
    sql
}

/// Parse a `CREATE FUNCTION … LANGUAGE plpgsql` statement's body with
/// PL/pgSQL's grammar, returning libpg_query's JSON rendering of it.
pub fn parse_plpgsql(sql: &str) -> Result<serde_json::Value> {
    let input = CString::new(sql)?;
    let result = unsafe { ffi::pg_query_parse_plpgsql(input.as_ptr()) };
    let parsed = if result.error.is_null() {
        let json = unsafe { CStr::from_ptr(result.plpgsql_funcs) }.to_string_lossy();
        serde_json::from_str(&json).map_err(|e| Error::InvalidJson(e.to_string()))
    } else {
        Err(Error::Parse(unsafe { error_message(result.error) }))
    };
    unsafe { ffi::pg_query_free_plpgsql_parse_result(result) };
    parsed
}

/// Every node reachable from `roots`, breadth-first, each with its depth
/// (the roots at 0).
fn walk(roots: Vec<NodeRef<'_>>) -> Vec<(NodeRef<'_>, usize)> {
    let mut queue: VecDeque<(NodeRef<'_>, usize)> = roots.into_iter().map(|n| (n, 0)).collect();
    let mut out = Vec::new();
    while let Some((n, depth)) = queue.pop_front() {
        out.push((n, depth));
        node::children(n, &mut |child| queue.push_back((child, depth + 1)));
    }
    out
}

/// [`walk`] over raw pointers.
///
/// # Safety
/// See [`protobuf::ParseResult::nodes_mut`].
unsafe fn walk_mut(roots: Vec<NodeMut>) -> Vec<(NodeMut, usize)> {
    let mut queue: VecDeque<(NodeMut, usize)> = roots.into_iter().map(|n| (n, 0)).collect();
    let mut out = Vec::new();
    while let Some((n, depth)) = queue.pop_front() {
        out.push((n, depth));
        unsafe { node::children_mut(n, &mut |child| queue.push_back((child, depth + 1))) };
    }
    out
}

impl protobuf::ParseResult {
    /// Every node of every statement, breadth-first, with its depth.
    pub fn nodes(&self) -> Vec<(NodeRef<'_>, usize)> {
        walk(
            self.stmts
                .iter()
                .filter_map(|s| s.stmt.as_ref()?.node.as_ref())
                .map(NodeEnum::to_ref)
                .collect(),
        )
    }

    /// [`Self::nodes`] as raw mutable pointers.
    ///
    /// # Safety
    /// The pointers are valid only while the tree is neither moved nor
    /// dropped, and a pointer into a subtree dangles once a mutation through
    /// another pointer replaces that subtree. The caller must not create
    /// overlapping references through them.
    pub unsafe fn nodes_mut(&mut self) -> Vec<(NodeMut, usize)> {
        let roots = self
            .stmts
            .iter_mut()
            .filter_map(|s| s.stmt.as_mut()?.node.as_mut())
            .map(NodeEnum::to_mut)
            .collect();
        unsafe { walk_mut(roots) }
    }

    /// The SQL this tree deparses to.
    pub fn deparse(&self) -> Result<String> {
        deparse(self)
    }
}

impl NodeEnum {
    /// This node and every node below it, breadth-first, with its depth
    /// (this node at 0).
    pub fn nodes(&self) -> Vec<(NodeRef<'_>, usize)> {
        walk(vec![self.to_ref()])
    }

    /// The SQL this node deparses to, as a statement of its own.
    pub fn deparse(&self) -> Result<String> {
        deparse(&protobuf::ParseResult {
            version: PG_VERSION_NUM,
            stmts: vec![protobuf::RawStmt {
                stmt: Some(Box::new(protobuf::Node {
                    node: Some(self.clone()),
                })),
                stmt_location: 0,
                stmt_len: 0,
            }],
        })
    }
}

impl protobuf::Node {
    /// The SQL this node deparses to, as a statement of its own.
    pub fn deparse(&self) -> Result<String> {
        match &self.node {
            Some(n) => n.deparse(),
            None => Err(Error::Parse("empty node".into())),
        }
    }
}
