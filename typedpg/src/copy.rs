//! Binary `COPY ... FROM STDIN`, behind the `copy_in!` macro.

use std::error::Error as StdError;
use std::pin::pin;

use bytes::{Bytes, BytesMut};
use tokio_postgres::CopyInSink;
use tokio_postgres::binary_copy::BinaryCopyInWriter;
use tokio_postgres::types::{IsNull, Kind, ToSql, Type, to_sql_checked};

/// One row's values, in the target columns' order.
pub type CopyRow = Vec<Box<dyn ToSql + Sync + Send>>;

/// The client-side types of the target columns: the statement `copy_in!`
/// generates (`SELECT NULL::<type>, ...`) is prepared only for them.
pub(crate) fn column_types(describe: &tokio_postgres::Statement) -> Vec<Type> {
    describe
        .columns()
        .iter()
        .map(|c| c.type_().clone())
        .collect()
}

/// Write `rows` through `sink` in the binary format and finish the COPY,
/// returning how many rows it took. An error (from a row or the server)
/// drops the sink unfinished, which aborts the COPY.
pub(crate) async fn write_rows<I>(
    sink: CopyInSink<Bytes>,
    types: &[Type],
    rows: I,
) -> Result<u64, crate::Error>
where
    I: Iterator<Item = Result<CopyRow, crate::Error>>,
{
    let mut writer = pin!(BinaryCopyInWriter::new(sink, types));
    for row in rows {
        let row = row?;
        let values: Vec<CopyValue<'_>> = row.iter().map(|v| CopyValue(v.as_ref())).collect();
        let refs: Vec<&(dyn ToSql + Sync)> =
            values.iter().map(|v| v as &(dyn ToSql + Sync)).collect();
        writer.as_mut().write(&refs).await?;
    }
    Ok(writer.as_mut().finish().await?)
}

/// A value encoded in its column's binary format. The macro has checked
/// the value's Rust type against the column at compile time; this only
/// steers the encoding past what `ToSql` impls accept:
///
/// - a domain's value is its base type's (so a `jsonb` domain gets jsonb's
///   version byte);
/// - an enum's is its label's text;
/// - an array of either is encoded as an array of the stand-in type, then
///   its header's element type OID (bytes 8..12 of the binary array
///   format) is set back to the column's, which `array_recv` checks.
struct CopyValue<'a>(&'a (dyn ToSql + Sync + Send));

impl std::fmt::Debug for CopyValue<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// The type to encode a value of `ty` as, if not `ty` itself.
fn stand_in(ty: &Type) -> Option<Type> {
    match ty.kind() {
        Kind::Domain(base) => Some(stand_in(base).unwrap_or_else(|| base.clone())),
        Kind::Enum(_) => Some(Type::TEXT),
        Kind::Array(member) => {
            let member = stand_in(member)?;
            Some(Type::new(
                format!("_{}", member.name()),
                0,
                Kind::Array(member),
                "pg_catalog".into(),
            ))
        }
        _ => None,
    }
}

impl ToSql for CopyValue<'_> {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn StdError + Sync + Send>> {
        // A domain's value is its base type's, on the wire too.
        let mut actual = ty;
        while let Kind::Domain(base) = actual.kind() {
            actual = base;
        }
        let Some(encode_as) = stand_in(actual) else {
            return self.0.to_sql_checked(actual, out);
        };
        let start = out.len();
        let is_null = self.0.to_sql_checked(&encode_as, out)?;
        if let (IsNull::No, Kind::Array(member)) = (&is_null, actual.kind()) {
            out[start + 8..start + 12].copy_from_slice(&member.oid().to_be_bytes());
        }
        Ok(is_null)
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    to_sql_checked!();
}
