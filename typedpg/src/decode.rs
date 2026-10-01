//! Reading the columns of a result row, with errors that name the column.
//!
//! The `sql!` macro has checked every column's Rust type against the query
//! at compile time, so a decoding failure here means the value itself can't
//! be represented (a NULL where the analysis proved none, a NULL array
//! element, a `numeric` NaN, an `infinity` timestamp…). That is an
//! [`Error::Deserialize`] naming the column, never a panic.

use std::error::Error as StdError;

use tokio_postgres::Row;
use tokio_postgres::types::{FromSql, Kind, Type, WasNull};

use crate::__private::BaseTyped;
use crate::Error;

/// Reads column `idx` of `row` as `T`. `column` and `pg_type` name the
/// column in the error (`pg_type` as the query's analysis typed it, which
/// keeps a domain's name where the row description only has its base).
pub fn read_column<'a, T: FromSql<'a>>(
    row: &'a Row,
    idx: usize,
    column: &str,
    pg_type: &str,
) -> Result<T, Error> {
    // `try_get` would check this too, but name the wrapper in its message.
    if let Some(c) = row.columns().get(idx)
        && !<BaseTyped<T> as FromSql<'a>>::accepts(c.type_())
    {
        return Err(Error::Deserialize(format!(
            "column \"{column}\" ({pg_type}): cannot convert between the Rust type `{}` and \
             the Postgres type `{}`",
            std::any::type_name::<T>(),
            c.type_().name(),
        )));
    }
    row.try_get::<_, BaseTyped<T>>(idx)
        .map(|v| v.0)
        .map_err(|e| column_error(row, idx, column, pg_type, &e))
}

/// Reads the column a `#[derive(FromRow)]` field named `field` maps to.
///
/// The column is the one named `field`, or else the one whose name is
/// `field` once a nullability annotation (`"title!"`, `"age?"`) is stripped
/// and the rest is made an identifier the way `sql!` names its output
/// struct's fields.
pub fn read_named_column<'a, T: FromSql<'a>>(row: &'a Row, field: &str) -> Result<T, Error> {
    let columns = row.columns();
    let idx = columns
        .iter()
        .position(|c| c.name() == field)
        .or_else(|| {
            columns
                .iter()
                .position(|c| field_name_of_column(c.name()) == field)
        })
        .ok_or_else(|| {
            Error::Deserialize(format!(
                "the query has no column named \"{field}\" (its columns: {})",
                columns
                    .iter()
                    .map(|c| format!("\"{}\"", c.name()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
    let pg_type = type_label(columns[idx].type_());
    read_column(row, idx, field, &pg_type)
}

/// The Rust identifier `sql!` gives the output field of a column named
/// `name`: a trailing nullability annotation dropped, every character that
/// can't be in an identifier replaced by `_`, a leading digit prefixed with
/// `_` (Rust keywords become raw identifiers, whose name is the same).
pub fn field_name_of_column(name: &str) -> String {
    let name = name
        .strip_suffix('!')
        .or_else(|| name.strip_suffix('?'))
        .unwrap_or(name);
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "_unnamed".to_string()
    } else if sanitized.starts_with(|c: char| c.is_ascii_digit()) {
        format!("_{sanitized}")
    } else {
        sanitized
    }
}

/// A type as the row description names it: `int4`, `text[]`.
fn type_label(ty: &Type) -> String {
    match ty.kind() {
        Kind::Array(member) => format!("{}[]", member.name()),
        _ => ty.name().to_string(),
    }
}

/// Any value, as its raw bytes: tells a NULL value from a value that
/// failed to decode because something inside it was NULL.
struct Raw<'a>(Option<&'a [u8]>);

impl<'a> FromSql<'a> for Raw<'a> {
    fn from_sql(_: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn StdError + Sync + Send>> {
        Ok(Raw(Some(raw)))
    }

    fn from_sql_null(_: &Type) -> Result<Self, Box<dyn StdError + Sync + Send>> {
        Ok(Raw(None))
    }

    fn accepts(_: &Type) -> bool {
        true
    }
}

fn column_error(
    row: &Row,
    idx: usize,
    column: &str,
    pg_type: &str,
    error: &tokio_postgres::Error,
) -> Error {
    let cause = StdError::source(error);
    if cause.is_some_and(|c| c.is::<WasNull>()) {
        let is_null = row.try_get::<_, Raw<'_>>(idx).is_ok_and(|r| r.0.is_none());
        let is_array = row
            .columns()
            .get(idx)
            .is_some_and(|c| matches!(c.type_().kind(), Kind::Array(_)));
        return Error::Deserialize(if is_null {
            format!(
                "column \"{column}\" ({pg_type}) is NULL, but its Rust type is not an Option \
                 (a \"{column}?\" alias makes sql! read it as one)"
            )
        } else if is_array {
            format!(
                "column \"{column}\" ({pg_type}) holds a NULL array element, but its elements \
                 are not known to be nullable (PostgreSQL does not constrain array elements): \
                 drop them with array_remove(..., NULL), or build the array so its elements' \
                 nullability is known"
            )
        } else {
            format!("column \"{column}\" ({pg_type}) holds a NULL inside its value")
        });
    }
    let mut message = format!("column \"{column}\" ({pg_type}): ");
    match cause {
        Some(cause) => {
            message.push_str(&cause.to_string());
            let mut next = cause.source();
            while let Some(c) = next {
                message.push_str(": ");
                message.push_str(&c.to_string());
                next = c.source();
            }
        }
        None => message.push_str(&error.to_string()),
    }
    Error::Deserialize(message)
}

#[cfg(test)]
mod tests {
    use super::field_name_of_column;

    #[test]
    fn field_names_follow_the_sql_macro() {
        assert_eq!(field_name_of_column("title!"), "title");
        assert_eq!(field_name_of_column("age?"), "age");
        assert_eq!(field_name_of_column("my col"), "my_col");
        assert_eq!(field_name_of_column("1st"), "_1st");
        assert_eq!(field_name_of_column("type"), "type");
        assert_eq!(field_name_of_column(""), "_unnamed");
    }
}
