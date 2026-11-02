//! Which tables have a TOAST table (`reltoastrelid`). PG makes one at the
//! end of a CREATE TABLE / CREATE TABLE AS / ALTER TABLE that leaves the
//! relation with columns needing it (AlterTableCreateToastTable →
//! needs_toast_table) and keeps it afterwards; ALTER TABLE SET / RESET
//! checks a `toast.` storage parameter only when there is one.

use super::*;
use crate::pg_catalog::TypAlign;

/// `TOAST_TUPLE_THRESHOLD` for the default 8 kB block size.
const TOAST_TUPLE_THRESHOLD: usize = 2032;
/// `SizeofHeapTupleHeader`.
const HEAP_TUPLE_HEADER_SIZE: usize = 23;
/// `MAXIMUM_ALIGNOF`.
const MAXALIGN: usize = 8;
/// `pg_encoding_max_length(PG_UTF8)`: the analyzer models UTF8 databases.
const UTF8_MAX_LENGTH: i32 = 4;
/// `VARHDRSZ`.
const VARHDRSZ: i32 = 4;
/// `bit` and `varbit` (pg_type.dat).
const BIT: PgTypeOid = PgTypeOid::from_raw(1560);
const VARBIT: PgTypeOid = PgTypeOid::from_raw(1562);

fn align(len: usize, to: usize) -> usize {
    len.div_ceil(to) * to
}

/// type_maximum_size (format_type.c): the largest value a column of
/// `typid` / `typmod` can hold, `None` when unbounded.
fn type_maximum_size(interp: &PgCatalog, typid: PgTypeOid, typmod: Option<i32>) -> Option<i32> {
    use crate::pg_catalog::oid;
    // A domain's column has no typmod of its own, and type_maximum_size
    // doesn't look through the domain: its size is unbounded.
    if interp
        .pg_type
        .get(&typid)
        .is_some_and(|t| t.typtype == TypType::Domain)
    {
        return None;
    }
    let typmod = typmod.filter(|m| *m >= 0)?;
    match typid {
        oid::BPCHAR | oid::VARCHAR => {
            // typmod includes VARHDRSZ; each character can take up to the
            // encoding's maximum length.
            Some((typmod - VARHDRSZ) * UTF8_MAX_LENGTH + VARHDRSZ)
        }
        oid::NUMERIC => {
            // numeric_maximum_size: NUMERIC_HDRSZ plus DEC_DIGITS-digit
            // groups of int16.
            let precision = ((typmod - VARHDRSZ) >> 16) & 0xffff;
            let digit_groups = (precision + 2 * (4 - 1)) / 4;
            Some(8 + digit_groups * 2)
        }
        BIT | VARBIT => {
            // VARHDRSZ + VARBITHDRSZ + the bytes of typmod bits.
            Some(VARHDRSZ + 4 + (typmod + 7) / 8)
        }
        _ => None,
    }
}

/// heapam_relation_needs_toast_table: some column is toastable, and a row
/// could exceed `TOAST_TUPLE_THRESHOLD`.
fn needs_toast_table(interp: &PgCatalog, relid: PgClassOid) -> bool {
    let attrs = interp.attributes_of(relid);
    let mut data_length = 0usize;
    let mut maxlength_unknown = false;
    let mut has_toastable_attrs = false;
    for attr in attrs {
        if attr.attgenerated == Some(AttGenerated::Virtual) {
            continue;
        }
        let Some(typ) = interp.pg_type.get(&attr.atttypid) else {
            continue;
        };
        let alignment = match typ.typalign {
            TypAlign::Char => 1,
            TypAlign::Short => 2,
            TypAlign::Int => 4,
            TypAlign::Double => 8,
        };
        data_length = align(data_length, alignment);
        if typ.typlen > 0 {
            data_length += typ.typlen as usize;
            continue;
        }
        match type_maximum_size(interp, attr.atttypid, attr.atttypmod) {
            Some(max) => data_length += max.max(0) as usize,
            None => maxlength_unknown = true,
        }
        let storage = interp
            .attr_storage
            .get(&(relid, attr.attnum))
            .copied()
            .unwrap_or(typ.typstorage);
        if storage != TypStorage::Plain {
            has_toastable_attrs = true;
        }
    }
    if !has_toastable_attrs {
        return false;
    }
    if maxlength_unknown {
        return true;
    }
    // BITMAPLEN(natts).
    let bitmap = attrs.len().div_ceil(8);
    let tuple_length =
        align(HEAP_TUPLE_HEADER_SIZE + bitmap, MAXALIGN) + align(data_length, MAXALIGN);
    tuple_length > TOAST_TUPLE_THRESHOLD
}

/// Record the TOAST tables the statements so far have made: every table
/// and materialized view whose columns need one has one. Called before a
/// statement that can remove columns or read the TOAST state — a column
/// the previous statements left in place still counts.
pub(crate) fn note_toast_tables(interp: &mut PgCatalog) {
    // A rewritten heap has a TOAST table only if its columns need one.
    let rewritten: Vec<PgClassOid> = interp.toast_rewrites.iter().copied().collect();
    interp.toast_rewrites.clear();
    for relid in rewritten {
        if needs_toast_table(interp, relid) {
            interp.toast_tables.insert(relid);
        } else {
            interp.toast_tables.remove(&relid);
        }
    }
    let tables: Vec<PgClassOid> = interp
        .pg_class
        .values()
        .filter(|c| matches!(c.relkind, RelKind::Table | RelKind::MaterializedView))
        .filter(|c| !interp.toast_tables.contains(&c.oid))
        .map(|c| c.oid)
        .collect();
    for relid in tables {
        if needs_toast_table(interp, relid) {
            interp.toast_tables.insert(relid);
        }
    }
}

/// A rewrite of `relid`'s heap (make_new_heap: ALTER TABLE's rewrite,
/// CLUSTER, VACUUM FULL, REFRESH MATERIALIZED VIEW) builds its TOAST table
/// anew — only if the columns then need one (NewHeapCreateToastTable), so
/// one that dropped, retyped or PLAIN-stored columns needed goes. Settled
/// at the next [`note_toast_tables`], once the statement's columns are
/// final.
pub(crate) fn heap_rewritten(interp: &mut PgCatalog, relid: PgClassOid) {
    interp.toast_rewrites.insert(relid);
}
