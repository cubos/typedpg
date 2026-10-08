//! `copy_in!`: a `COPY table (columns) FROM STDIN` target, checked against
//! the catalog as PG's `CopyGetAttnums` / `CopyFrom` do, with the type and
//! nullability each column's values must have.

use typedpg_pg_query::protobuf::node;

use super::AnalyzedColumn;
use crate::error::AnalyzeError;
use crate::pg_catalog::PgCatalog;

/// What `copy_in!` writes: the statement to run and, in order, the columns
/// each row supplies.
#[derive(Debug, Clone)]
pub struct AnalyzedCopyIn {
    /// `COPY <table> (<columns>) FROM STDIN (FORMAT binary)`, every name
    /// qualified and quoted as needed.
    pub copy_sql: String,
    /// `SELECT NULL::<type>, ...` over the columns' types: preparing it
    /// tells the client each column's type (user-defined ones included)
    /// for the binary format, without reading the table — `COPY FROM`
    /// needs only INSERT privilege.
    pub describe_sql: String,
    /// The target columns, in the order each row's values come. `nullable`
    /// is whether a value may be NULL: false for a NOT NULL column (even a
    /// NOT VALID constraint checks new rows) or a NOT NULL domain.
    pub columns: Vec<AnalyzedColumn>,
}

/// A grammar error in the `COPY <target> FROM STDIN` built around a
/// `copy_in!` target, reported against the target as written: the
/// `Invalid statement:` wrapper and the statement the user never wrote are
/// dropped, and the caret goes on the target (`prefix` is the length of
/// what precedes it). An error past the target's end — in the appended
/// `FROM STDIN` — is the target ending too early: `syntax error at end of
/// input`, as PG says when the input ends there.
fn copy_target_syntax_error(
    e: typedpg_pg_query::Error,
    target: &str,
    prefix: usize,
) -> AnalyzeError {
    let typedpg_pg_query::Error::Parse { message, position } = e else {
        return AnalyzeError::Invalid(e.to_string());
    };
    let lex = crate::param::LexOutput::identity(target);
    let _guard = crate::error::DiagContextGuard::install(target, &lex);
    let at = position.map(|p| p.saturating_sub(prefix));
    let (message, span) = match at {
        Some(at) if at < target.len() => {
            let span = crate::error::SourceSpan::syntax_error_at(target, at, &message);
            (message, Some(span))
        }
        Some(_) => (
            "syntax error at end of input".to_owned(),
            Some(crate::error::SourceSpan::one_char_at(target.len())),
        ),
        None => (message, None),
    };
    crate::error::RawError::new(
        AnalyzeError::Parse(message),
        span,
        Some(
            "a copy_in! target is a table and an optional column list: `table (column, ...)`"
                .into(),
        ),
    )
    .finalize_implicit()
}

impl PgCatalog {
    /// Check `target` — `table` or `table (column, ...)`, as written after
    /// `COPY` — as the target of a `COPY ... FROM STDIN`.
    pub fn analyze_copy_in(&self, target: &str) -> Result<AnalyzedCopyIn, AnalyzeError> {
        let invalid = |msg: String| AnalyzeError::Invalid(msg);
        const PREFIX: &str = "COPY ";
        let sql = format!("{PREFIX}{target} FROM STDIN");
        let parsed = typedpg_pg_query::parse(&sql)
            .map_err(|e| copy_target_syntax_error(e, target, PREFIX.len()))?;
        // Exactly the one statement, with nothing but a relation and a
        // column list: the target can't smuggle in options or more SQL.
        let [raw] = parsed.protobuf.stmts.as_slice() else {
            return Err(invalid(format!(
                "copy_in! target must be a table and an optional column list, got `{target}`"
            )));
        };
        let Some(node::Node::CopyStmt(stmt)) = raw.stmt.as_ref().and_then(|n| n.node.as_ref())
        else {
            return Err(invalid(format!("`COPY {target}` is not a COPY statement")));
        };
        if stmt.relation.is_none()
            || stmt.query.is_some()
            || !stmt.is_from
            || stmt.is_program
            || !stmt.filename.is_empty()
            || !stmt.options.is_empty()
            || stmt.where_clause.is_some()
        {
            return Err(invalid(format!(
                "copy_in! target must be a table and an optional column list, got `{target}`"
            )));
        }
        crate::ddl::maintenance::copy(self, stmt).map_err(|e| invalid(e.to_string()))?;

        let rv = stmt.relation.as_ref().expect("checked above");
        let (_, relid) =
            crate::ddl::util::lookup_relation(self, rv).map_err(|e| invalid(e.to_string()))?;
        let class = self
            .pg_class
            .get(&relid)
            .ok_or_else(|| AnalyzeError::Internal("COPY target without pg_class row".into()))?;
        let attrs = self.attributes_of(relid);
        let names: Vec<&str> = stmt
            .attlist
            .iter()
            .filter_map(crate::ddl::util::node_string)
            .collect();
        // CopyGetAttnums: no list means every column but the generated ones.
        let targets: Vec<&crate::pg_catalog::PgAttribute> = if names.is_empty() {
            attrs.iter().filter(|a| a.attgenerated.is_none()).collect()
        } else {
            names
                .iter()
                .filter_map(|n| attrs.iter().find(|a| a.attname == *n))
                .collect()
        };

        let mut columns = Vec::with_capacity(targets.len());
        for attr in &targets {
            columns.push(AnalyzedColumn {
                name: attr.attname.clone(),
                pg_type: super::type_resolution::column_type(
                    attr.atttypid,
                    self.effective_typmod(attr.atttypid, attr.atttypmod),
                    attr.attcollation,
                    self,
                )?,
                nullable: !(attr.attnotnull || self.type_is_not_null(attr.atttypid)),
                refinement: crate::refine::Refinement::NONE,
            });
        }

        let schema = self.namespace_name(class.relnamespace).unwrap_or("public");
        let table = typedpg_core::QualifiedName::new(schema, &class.relname);
        let column_list: Vec<String> = targets
            .iter()
            .map(|a| typedpg_core::quote_identifier(&a.attname))
            .collect();
        let mut type_names = Vec::with_capacity(targets.len());
        for attr in &targets {
            let ty = self.pg_type.get(&attr.atttypid).ok_or_else(|| {
                AnalyzeError::Internal(format!("column type {} not in pg_type", attr.atttypid))
            })?;
            let schema = self.namespace_name(ty.typnamespace).unwrap_or("pg_catalog");
            type_names.push(format!(
                "NULL::{}",
                typedpg_core::QualifiedName::new(schema, &ty.typname)
            ));
        }
        Ok(AnalyzedCopyIn {
            describe_sql: format!("SELECT {}", type_names.join(", ")),
            copy_sql: format!(
                "COPY {table} ({}) FROM STDIN (FORMAT binary)",
                column_list.join(", ")
            ),
            columns,
        })
    }
}
