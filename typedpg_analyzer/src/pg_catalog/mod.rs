//! In-memory mirror of PostgreSQL's `pg_catalog` schema.
//!
//! Each table here corresponds 1:1 to a real `pg_catalog` table — same name,
//! same column names (literal PG identifiers like `typname`, `relkind`,
//! `prokind`, …), same FK semantics. Schemas are referenced by OID through
//! `pg_namespace`; views' SELECT ASTs and extension membership are tracked
//! through `pg_class.relviewdef` and `pg_depend` rather than ad-hoc fields on
//! the schema entries.
//!
//! On top of the tables, [`PgCatalog`] keeps a handful of name-keyed
//! HashMap indexes that are rebuilt from the rows in [`PgCatalog::from_seed`]
//! and kept in sync by the DDL handlers via the `insert_pg_*` /
//! `remove_pg_*` helpers.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::ddl::{DdlError, apply_sql_to};
use crate::error::AnalyzeError;
use crate::lexer::lex;
use crate::oid::{
    PgCastOid, PgClassOid, PgCollationOid, PgConstraintOid, PgExtensionOid, PgGenericOid,
    PgNamespaceOid, PgOperatorOid, PgProcOid, PgRewriteOid, PgTypeOid,
};
use crate::resolve::{AnalyzedQuery, analyze_static, build_spread_sample_sql, fuse};
use crate::seed::load_seed;

// ─── Built-in type OIDs ────────────────────────────────────────────────────
//
// Stable OIDs PG hard-codes for builtin types. Used by the analyzer to drive
// coercion, operator resolution, and pseudo-type detection.

pub(crate) mod oid {
    use crate::oid::PgTypeOid;

    pub const BOOL: PgTypeOid = PgTypeOid::from_raw(16);
    pub const NAME: PgTypeOid = PgTypeOid::from_raw(19);
    pub const INT8: PgTypeOid = PgTypeOid::from_raw(20);
    pub const INT2: PgTypeOid = PgTypeOid::from_raw(21);
    pub const INT4: PgTypeOid = PgTypeOid::from_raw(23);
    pub const TEXT: PgTypeOid = PgTypeOid::from_raw(25);
    pub const OID: PgTypeOid = PgTypeOid::from_raw(26);
    pub const TID: PgTypeOid = PgTypeOid::from_raw(27);
    pub const XID: PgTypeOid = PgTypeOid::from_raw(28);
    pub const CID: PgTypeOid = PgTypeOid::from_raw(29);
    pub const FLOAT4: PgTypeOid = PgTypeOid::from_raw(700);
    pub const FLOAT8: PgTypeOid = PgTypeOid::from_raw(701);
    pub const UNKNOWN: PgTypeOid = PgTypeOid::from_raw(705);
    pub const BPCHAR: PgTypeOid = PgTypeOid::from_raw(1042);
    pub const VARCHAR: PgTypeOid = PgTypeOid::from_raw(1043);
    pub const DATE: PgTypeOid = PgTypeOid::from_raw(1082);
    pub const TIME: PgTypeOid = PgTypeOid::from_raw(1083);
    pub const TIMESTAMP: PgTypeOid = PgTypeOid::from_raw(1114);
    pub const TIMESTAMPTZ: PgTypeOid = PgTypeOid::from_raw(1184);
    pub const TIMETZ: PgTypeOid = PgTypeOid::from_raw(1266);
    pub const NUMERIC: PgTypeOid = PgTypeOid::from_raw(1700);
    pub const RECORD: PgTypeOid = PgTypeOid::from_raw(2249);
}

/// PG's hidden system columns, present on every table/view. Negative attnums
/// match PG's `pg_attribute` convention. Excluded from `SELECT *` expansion
/// and from "did you mean" suggestions, but resolvable when named explicitly.
pub const SYSTEM_COLUMNS: &[(&str, PgTypeOid, i16)] = &[
    ("tableoid", oid::OID, -6),
    ("cmax", oid::CID, -5),
    ("xmax", oid::XID, -4),
    ("cmin", oid::CID, -3),
    ("xmin", oid::XID, -2),
    ("ctid", oid::TID, -1),
];

// ─── Catalog table OIDs ────────────────────────────────────────────────────
//
// Stable OIDs PG hard-codes for the catalog tables themselves. Used as
// `classid` / `refclassid` in `pg_depend` rows so that, e.g., a view's
// dependency on a table reads as `classid = PG_CLASS_RELID, objid = view_oid,
// refclassid = PG_CLASS_RELID, refobjid = table_oid`.

pub const PG_NAMESPACE_RELID: PgClassOid = PgClassOid::from_raw(2615);
pub const PG_TYPE_RELID: PgClassOid = PgClassOid::from_raw(1247);
pub const PG_CLASS_RELID: PgClassOid = PgClassOid::from_raw(1259);
pub const PG_PROC_RELID: PgClassOid = PgClassOid::from_raw(1255);
pub const PG_OPERATOR_RELID: PgClassOid = PgClassOid::from_raw(2617);
pub const PG_CAST_RELID: PgClassOid = PgClassOid::from_raw(2605);
pub const PG_EXTENSION_RELID: PgClassOid = PgClassOid::from_raw(3079);

mod rows;
pub use rows::*;

// ─── Seed (on-disk shape) ──────────────────────────────────────────────────

/// On-disk shape of `seed.json`. Each catalog table is a flat `Vec<Row>`
/// ordered by OID (or by `(attrelid, attnum)` for `pg_attribute`, by
/// `(enumtypid, enumsortorder)` for `pg_enum`, by `(classid, objid)` for
/// `pg_depend`). `serde_json` round-trips this directly — no `BTreeMap`
/// re-ordering needed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PgCatalogSeed {
    #[serde(default)]
    pub pg_namespace: Vec<PgNamespace>,
    #[serde(default)]
    pub pg_type: Vec<PgType>,
    #[serde(default)]
    pub pg_enum: Vec<PgEnum>,
    #[serde(default)]
    pub pg_range: Vec<PgRange>,
    #[serde(default)]
    pub pg_class: Vec<PgClass>,
    #[serde(default)]
    pub pg_attribute: Vec<PgAttribute>,
    #[serde(default)]
    pub pg_proc: Vec<PgProc>,
    #[serde(default)]
    pub pg_aggregate: Vec<PgAggregate>,
    #[serde(default)]
    pub pg_operator: Vec<PgOperator>,
    #[serde(default)]
    pub pg_cast: Vec<PgCast>,
    #[serde(default)]
    pub pg_extension: Vec<PgExtension>,
    #[serde(default)]
    pub pg_depend: Vec<PgDepend>,
    #[serde(default)]
    pub pg_inherits: Vec<PgInherits>,
    #[serde(default)]
    pub pg_constraint: Vec<PgConstraint>,
    #[serde(default)]
    pub pg_index: Vec<PgIndex>,
    #[serde(default)]
    pub pg_rewrite: Vec<PgRewrite>,
    #[serde(default)]
    pub pg_collation: Vec<PgCollation>,
    /// Namespace OIDs in search order. Non-PG (PG keeps this in a GUC).
    #[serde(default, with = "crate::oid::vec_oid")]
    pub search_path: Vec<PgNamespaceOid>,
    /// `pg_get_functiondef` of every `LANGUAGE sql` function, so the ones
    /// PG would inline (`inline_function`) count by their body — e.g. the
    /// STABLE `textanycat` behind `text || anynonarray`.
    #[serde(default)]
    pub sql_function_defs: Vec<(PgProcOid, String)>,
    #[serde(default)]
    pub pg_am: Vec<PgAm>,
    #[serde(default)]
    pub pg_opfamily: Vec<PgOpfamily>,
    #[serde(default)]
    pub pg_opclass: Vec<PgOpclass>,
}

// ─── In-memory catalog ─────────────────────────────────────────────────────

/// Mutable in-memory PostgreSQL catalog.
///
/// Starts from the embedded PG18 seed and evolves by applying DDL via
/// [`PgCatalog::apply_sql`]. Each catalog table is stored as a HashMap keyed
/// by primary OID (or by FK in the case of `pg_attribute`/`pg_enum`/
/// `pg_range`); name-keyed indexes are rebuilt at construction time and kept
/// in sync by the DDL handlers via the `insert_pg_*` / `remove_pg_*` helpers
/// in the construction impl block below.
#[derive(Clone)]
pub struct PgCatalog {
    // ── Catalog tables ──
    pub(crate) pg_namespace: HashMap<PgNamespaceOid, PgNamespace>,
    pub(crate) pg_type: HashMap<PgTypeOid, PgType>,
    pub(crate) pg_class: HashMap<PgClassOid, PgClass>,
    pub(crate) pg_proc: HashMap<PgProcOid, PgProc>,
    pub(crate) pg_aggregate: HashMap<PgProcOid, PgAggregate>,
    pub(crate) pg_operator: HashMap<PgOperatorOid, PgOperator>,
    pub(crate) pg_cast: HashMap<PgCastOid, PgCast>,
    pub(crate) pg_extension: HashMap<PgExtensionOid, PgExtension>,
    /// Keyed by `attrelid`; vec ordered by `attnum`.
    pub(crate) pg_attribute: HashMap<PgClassOid, Vec<PgAttribute>>,
    /// Keyed by `enumtypid`; vec ordered by `enumsortorder`.
    pub(crate) pg_enum: HashMap<PgTypeOid, Vec<PgEnum>>,
    /// Keyed by `rngtypid`.
    pub(crate) pg_range: HashMap<PgTypeOid, PgRange>,
    pub(crate) pg_depend: Vec<PgDepend>,
    pub(crate) pg_inherits: Vec<PgInherits>,
    pub(crate) pg_constraint: HashMap<PgConstraintOid, PgConstraint>,
    /// Keyed by `indexrelid` (the index's `pg_class.oid`).
    pub(crate) pg_index: HashMap<PgClassOid, PgIndex>,
    pg_rewrite: HashMap<PgRewriteOid, PgRewrite>,
    pub(crate) pg_collation: HashMap<PgCollationOid, PgCollation>,
    pub(crate) pg_am: Vec<PgAm>,
    pub(crate) pg_opfamily: Vec<PgOpfamily>,
    pub(crate) pg_opclass: Vec<PgOpclass>,

    // ── Name-keyed indexes (built by `from_seed`, maintained by DDL) ──
    pub(crate) namespace_by_name: HashMap<String, PgNamespaceOid>,
    /// `(typnamespace, typname) -> typoid`.
    pub(crate) type_by_qname: HashMap<(PgNamespaceOid, String), PgTypeOid>,
    /// `(relnamespace, relname) -> classoid`.
    pub(crate) class_by_qname: HashMap<(PgNamespaceOid, String), PgClassOid>,
    /// `(pronamespace, proname) -> [procoid]` (overloads).
    pub(crate) proc_by_qname: HashMap<(PgNamespaceOid, String), Vec<PgProcOid>>,
    /// `(oprnamespace, oprname) -> [opoid]` (overloads).
    pub(crate) operator_by_qname: HashMap<(PgNamespaceOid, String), Vec<PgOperatorOid>>,
    /// `(castsource, casttarget) -> castoid`.
    pub(crate) cast_by_pair: HashMap<(PgTypeOid, PgTypeOid), PgCastOid>,
    pub(crate) extension_by_name: HashMap<String, PgExtensionOid>,
    /// `(collnamespace, collname) -> collation_oid`. Walked by analyzer
    /// when validating `COLLATE "x"` decorations.
    pub(crate) collation_by_qname: HashMap<(PgNamespaceOid, String), PgCollationOid>,

    // ── Session state (non-PG) ──
    /// Namespace OIDs in search order (analog of PG's `search_path` GUC).
    pub(crate) search_path: Vec<PgNamespaceOid>,
    /// Textual `search_path` settings behind [`Self::search_path`], which is
    /// re-derived from them whenever they or the set of schemas change —
    /// PG keeps the GUC as a list of names and resolves it lazily, so a path
    /// may name a schema that is only created later.
    pub(crate) search_path_guc: crate::ddl::session::SearchPathGuc,
    /// Named constraints of user domains (PG keeps them in `pg_constraint`
    /// with `contypid` set): what `ALTER DOMAIN ... DROP CONSTRAINT name`
    /// resolves against. The domain's effective NOT NULL lives in
    /// `pg_type.typnotnull`.
    pub(crate) domain_constraints: HashMap<PgTypeOid, Vec<crate::ddl::types::DomainConstraint>>,
    /// Type of each column DEFAULT expression as `strip_implicit_coercions`
    /// sees it (the expression's own type, before the coercion to the
    /// column type that `cookDefault` adds) — what ALTER COLUMN TYPE
    /// re-coerces (PG keeps the expression in `pg_attrdef`).
    pub(crate) attr_default_types: HashMap<(PgClassOid, i16), PgTypeOid>,
    /// The `check_function_bodies` GUC (pg_dump output turns it off).
    pub(crate) check_function_bodies: bool,
    /// The single expression of each inlinable `LANGUAGE sql` function
    /// (`SELECT expr` / `RETURN expr`, as `inline_function` requires),
    /// which PG substitutes for the call before checking an index or
    /// generation expression's mutability.
    pub(crate) inline_sql_bodies: HashMap<PgProcOid, pg_query::protobuf::Node>,
    /// Seeded `pg_get_functiondef` sources behind `inline_sql_bodies`,
    /// kept so `to_seed` round-trips them.
    sql_function_defs: HashMap<PgProcOid, String>,
    /// `pg_partitioned_table.partattrs` of each partitioned table: the
    /// partition key's attnums, `0` for an expression.
    pub(crate) partition_keys: HashMap<PgClassOid, Vec<i16>>,
    /// `pg_trigger`: each relation's triggers (name and function).
    pub(crate) triggers: HashMap<PgClassOid, Vec<crate::ddl::triggers::Trigger>>,
    /// `pg_policy`: each relation's row-security policy names.
    pub(crate) policies: HashMap<PgClassOid, Vec<String>>,
    /// `pg_rewrite` rule names added by CREATE RULE, per relation.
    pub(crate) rules: HashMap<PgClassOid, Vec<String>>,
    /// `pg_class.reloftype` of typed tables (`CREATE TABLE ... OF type`).
    pub(crate) typed_tables: HashMap<PgClassOid, PgTypeOid>,
    /// The `indisclustered` index of each table (CLUSTER ... USING,
    /// ALTER TABLE ... CLUSTER ON).
    pub(crate) clustered_indexes: HashMap<PgClassOid, PgClassOid>,
    /// `pg_statistic_ext`: extended statistics objects.
    pub(crate) statistics: Vec<crate::ddl::statistics::StatisticsObject>,
    /// `pg_partitioned_table`: strategy and key types.
    pub(crate) partition_specs: HashMap<PgClassOid, crate::ddl::tables::partbound::PartSpec>,
    /// `relpartbound` of each partition.
    pub(crate) partition_bounds: HashMap<PgClassOid, crate::ddl::tables::partbound::Bound>,
    /// CHECK constraints' expressions and `connoinherit`.
    pub(crate) check_defs: HashMap<PgConstraintOid, crate::ddl::tables::check_inherit::CheckDef>,
    /// `pg_type.typsubscript` of user base types, as the handler function's
    /// name (`hstore_subscript_handler`), set by `CREATE TYPE (SUBSCRIPT =
    /// …)` / `ALTER TYPE … SET (SUBSCRIPT = …)`. Built-in types aren't
    /// listed: theirs follow from `typelem` (the array handlers) and jsonb.
    pub(crate) type_subscript: HashMap<PgTypeOid, String>,
    next_oid: std::num::NonZeroU32,

    /// Lazy-initialized PG sanity mirror used by the `pg_sanity` feature to
    /// cross-check `apply_sql` / `analyze` against a real PG protocol server.
    /// `None` means the catalog was built from a non-default seed (via
    /// [`PgCatalog::from_seed`]) where no synced live server is available.
    /// The outer `Arc` lets `Clone` share one server across catalog clones —
    /// only the macro caches a clone, and it never enables `pg_sanity`.
    #[cfg(feature = "pg_sanity")]
    pg_sanity: Option<
        std::sync::Arc<std::sync::OnceLock<std::sync::Mutex<crate::pg_sanity::PgSanityServer>>>,
    >,

    /// Set once any `CREATE/ALTER/DROP EXTENSION` statement has touched the
    /// catalog. Our embedded extension SQL diverges from what PG sanity ships
    /// natively, so once an extension is in play the sanity check skips
    /// every later mirror call to avoid spurious panics.
    #[cfg(feature = "pg_sanity")]
    pg_sanity_tainted: bool,

    /// Per-catalog opt-out: tests that exercise behavior PG sanity can't
    /// validate (e.g. ON CONFLICT planner-level checks that the wire-level
    /// `prepare` doesn't reach) flip this on via [`PgCatalog::skip_pg_sanity`]
    /// to silence the sanity check without giving up the compile-time
    /// analyzer assertion.
    #[cfg(feature = "pg_sanity")]
    pg_sanity_skip: bool,
}

/// Starting OID for user-defined objects. Well above PG system OIDs (~16384).
const USER_OID_START: u32 = 100_000;

/// A relation and its columns — `(relname, [(attname, atttypid, attnotnull)])`
/// — as surfaced by [`PgCatalog::iter_relations`] for the differential fuzzer.
#[cfg(any(test, feature = "internal"))]
pub type FuzzRelation = (String, Vec<(String, PgTypeOid, bool)>);

/// `USER_OID_START` as a [`NonZeroU32`]. Validated at compile time so the
/// `next_oid` counter can be a `NonZeroU32` from construction onward without
/// a runtime check at each `alloc_oid` call.
const USER_OID_START_NZ: std::num::NonZeroU32 = match std::num::NonZeroU32::new(USER_OID_START) {
    Some(v) => v,
    None => panic!("USER_OID_START must be non-zero"),
};

// ─── Construction ──────────────────────────────────────────────────────────

impl PgCatalog {
    /// Create a catalog seeded with the embedded PG18 snapshot.
    ///
    /// Returns an [`AnalyzeError`] only if the embedded `seed.json` fails to
    /// deserialize — a build-time invariant of the analyzer crate that would
    /// only fire on a corrupted seed regeneration. Surfacing it as an error
    /// (rather than a panic) lets host processes such as the `sql!` macro
    /// report the failure cleanly.
    pub fn new() -> Result<Self, AnalyzeError> {
        #[cfg(not(feature = "pg_sanity"))]
        {
            Ok(Self::from_seed(load_seed()?))
        }
        #[cfg(feature = "pg_sanity")]
        {
            // Only `new()` (default seed) gets a PG sanity mirror — `from_seed`
            // with arbitrary seeds can't be reproduced on a fresh PG sanity
            // instance.
            let mut cat = Self::from_seed(load_seed()?);
            cat.pg_sanity = Some(std::sync::Arc::new(std::sync::OnceLock::new()));
            Ok(cat)
        }
    }

    /// Build a catalog from a serialized seed. `next_oid` is set to one past
    /// the max OID seen in the seed (or [`USER_OID_START`] if higher), so
    /// freshly allocated user objects can never collide with PG18's
    /// information_schema tables (which sit above 100k).
    pub fn from_seed(seed: PgCatalogSeed) -> Self {
        let mut cat = Self::empty();

        let mut max_oid: u32 = USER_OID_START;
        let bump = |oid: u32, m: &mut u32| {
            if oid > *m {
                *m = oid;
            }
        };
        for n in &seed.pg_namespace {
            bump(n.oid.get(), &mut max_oid);
        }
        for t in &seed.pg_type {
            bump(t.oid.get(), &mut max_oid);
        }
        for c in &seed.pg_class {
            bump(c.oid.get(), &mut max_oid);
        }
        for p in &seed.pg_proc {
            bump(p.oid.get(), &mut max_oid);
        }
        for o in &seed.pg_operator {
            bump(o.oid.get(), &mut max_oid);
        }
        for c in &seed.pg_cast {
            bump(c.oid.get(), &mut max_oid);
        }
        for e in &seed.pg_extension {
            bump(e.oid.get(), &mut max_oid);
        }
        for e in &seed.pg_enum {
            bump(e.oid.get(), &mut max_oid);
        }
        // `max_oid` always carries a value >= `USER_OID_START` (the seed
        // bumper started there), so the `+1` cannot wrap and the result is
        // always >= `USER_OID_START + 1` — non-zero by construction.
        cat.next_oid = std::num::NonZeroU32::new(max_oid.saturating_add(1).max(USER_OID_START))
            .unwrap_or(USER_OID_START_NZ);

        for ns in seed.pg_namespace {
            cat.namespace_by_name.insert(ns.nspname.clone(), ns.oid);
            cat.pg_namespace.insert(ns.oid, ns);
        }
        for t in seed.pg_type {
            cat.type_by_qname
                .insert((t.typnamespace, t.typname.clone()), t.oid);
            cat.pg_type.insert(t.oid, t);
        }
        for c in seed.pg_class {
            cat.class_by_qname
                .insert((c.relnamespace, c.relname.clone()), c.oid);
            cat.pg_class.insert(c.oid, c);
        }
        for a in seed.pg_attribute {
            cat.pg_attribute.entry(a.attrelid).or_default().push(a);
        }
        for v in cat.pg_attribute.values_mut() {
            v.sort_by_key(|a| a.attnum);
        }
        for e in seed.pg_enum {
            cat.pg_enum.entry(e.enumtypid).or_default().push(e);
        }
        for v in cat.pg_enum.values_mut() {
            v.sort_by(|a, b| {
                a.enumsortorder
                    .partial_cmp(&b.enumsortorder)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        for r in seed.pg_range {
            cat.pg_range.insert(r.rngtypid, r);
        }
        for p in seed.pg_proc {
            cat.proc_by_qname
                .entry((p.pronamespace, p.proname.clone()))
                .or_default()
                .push(p.oid);
            cat.pg_proc.insert(p.oid, p);
        }
        for a in seed.pg_aggregate {
            cat.pg_aggregate.insert(a.aggfnoid, a);
        }
        for o in seed.pg_operator {
            cat.operator_by_qname
                .entry((o.oprnamespace, o.oprname.clone()))
                .or_default()
                .push(o.oid);
            cat.pg_operator.insert(o.oid, o);
        }
        for c in seed.pg_cast {
            cat.cast_by_pair.insert((c.castsource, c.casttarget), c.oid);
            cat.pg_cast.insert(c.oid, c);
        }
        for e in seed.pg_extension {
            cat.extension_by_name.insert(e.extname.clone(), e.oid);
            cat.pg_extension.insert(e.oid, e);
        }
        cat.pg_depend = seed.pg_depend;
        cat.pg_inherits = seed.pg_inherits;
        for c in seed.pg_constraint {
            cat.pg_constraint.insert(c.oid, c);
        }
        for i in seed.pg_index {
            cat.pg_index.insert(i.indexrelid, i);
        }
        for r in seed.pg_rewrite {
            cat.pg_rewrite.insert(r.oid, r);
        }
        for c in seed.pg_collation {
            cat.collation_by_qname
                .insert((c.collnamespace, c.collname.clone()), c.oid);
            cat.pg_collation.insert(c.oid, c);
        }
        cat.search_path = seed.search_path;
        cat.pg_am = seed.pg_am;
        cat.pg_opfamily = seed.pg_opfamily;
        cat.pg_opclass = seed.pg_opclass;
        for (oid, definition) in seed.sql_function_defs {
            cat.add_sql_function_def(oid, definition);
        }
        cat.search_path_guc = crate::ddl::session::SearchPathGuc::with_default(
            cat.search_path
                .iter()
                .filter_map(|&oid| cat.namespace_name(oid).map(str::to_owned))
                .collect(),
        );
        cat
    }

    /// Build an empty catalog (no seed). Reserved for the view-AST rewrite
    /// walker, which needs a placeholder handle when descending into
    /// subselects whose RangeVars are already fully qualified.
    fn empty() -> Self {
        Self {
            pg_namespace: HashMap::new(),
            pg_type: HashMap::new(),
            pg_class: HashMap::new(),
            pg_proc: HashMap::new(),
            pg_aggregate: HashMap::new(),
            pg_operator: HashMap::new(),
            pg_cast: HashMap::new(),
            pg_extension: HashMap::new(),
            pg_attribute: HashMap::new(),
            pg_enum: HashMap::new(),
            pg_range: HashMap::new(),
            pg_depend: Vec::new(),
            pg_inherits: Vec::new(),
            pg_constraint: HashMap::new(),
            pg_index: HashMap::new(),
            pg_rewrite: HashMap::new(),
            pg_collation: HashMap::new(),
            pg_am: Vec::new(),
            pg_opfamily: Vec::new(),
            pg_opclass: Vec::new(),
            namespace_by_name: HashMap::new(),
            type_by_qname: HashMap::new(),
            class_by_qname: HashMap::new(),
            proc_by_qname: HashMap::new(),
            operator_by_qname: HashMap::new(),
            cast_by_pair: HashMap::new(),
            extension_by_name: HashMap::new(),
            collation_by_qname: HashMap::new(),
            search_path: Vec::new(),
            search_path_guc: Default::default(),
            domain_constraints: HashMap::new(),
            attr_default_types: HashMap::new(),
            check_function_bodies: true,
            inline_sql_bodies: HashMap::new(),
            sql_function_defs: HashMap::new(),
            partition_keys: HashMap::new(),
            triggers: HashMap::new(),
            policies: HashMap::new(),
            rules: HashMap::new(),
            typed_tables: HashMap::new(),
            clustered_indexes: HashMap::new(),
            statistics: Vec::new(),
            partition_specs: HashMap::new(),
            partition_bounds: HashMap::new(),
            check_defs: HashMap::new(),
            type_subscript: HashMap::new(),
            next_oid: USER_OID_START_NZ,
            #[cfg(feature = "pg_sanity")]
            pg_sanity: None,
            #[cfg(feature = "pg_sanity")]
            pg_sanity_tainted: false,
            #[cfg(feature = "pg_sanity")]
            pg_sanity_skip: false,
        }
    }

    /// Snapshot the catalog into a JSON-friendly seed. Round-trips with
    /// [`PgCatalog::from_seed`]. The OID allocator and any non-table state
    /// (namespace_by_name and friends) are not part of the seed.
    pub fn to_seed(&self) -> PgCatalogSeed {
        let mut pg_namespace: Vec<_> = self.pg_namespace.values().cloned().collect();
        pg_namespace.sort_by_key(|n| n.oid);

        let mut pg_type: Vec<_> = self.pg_type.values().cloned().collect();
        pg_type.sort_by_key(|t| t.oid);

        let mut pg_enum: Vec<_> = self
            .pg_enum
            .values()
            .flat_map(|v| v.iter().cloned())
            .collect();
        pg_enum.sort_by(|a, b| {
            a.enumtypid.cmp(&b.enumtypid).then(
                a.enumsortorder
                    .partial_cmp(&b.enumsortorder)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
        });

        let mut pg_range: Vec<_> = self.pg_range.values().cloned().collect();
        pg_range.sort_by_key(|r| r.rngtypid);

        let mut pg_class: Vec<_> = self.pg_class.values().cloned().collect();
        pg_class.sort_by_key(|c| c.oid);

        let mut pg_attribute: Vec<_> = self
            .pg_attribute
            .values()
            .flat_map(|v| v.iter().cloned())
            .collect();
        pg_attribute.sort_by_key(|a| (a.attrelid, a.attnum));

        let mut pg_proc: Vec<_> = self.pg_proc.values().cloned().collect();
        pg_proc.sort_by_key(|p| p.oid);

        let mut pg_aggregate: Vec<_> = self.pg_aggregate.values().cloned().collect();
        pg_aggregate.sort_by_key(|a| a.aggfnoid);

        let mut pg_operator: Vec<_> = self.pg_operator.values().cloned().collect();
        pg_operator.sort_by_key(|o| o.oid);

        let mut pg_cast: Vec<_> = self.pg_cast.values().cloned().collect();
        pg_cast.sort_by_key(|c| c.oid);

        let mut pg_extension: Vec<_> = self.pg_extension.values().cloned().collect();
        pg_extension.sort_by_key(|e| e.oid);

        let mut pg_depend = self.pg_depend.clone();
        pg_depend.sort_by_key(|d| (d.classid, d.objid, d.objsubid, d.refclassid, d.refobjid));

        let mut pg_inherits = self.pg_inherits.clone();
        pg_inherits.sort_by_key(|i| (i.inhrelid, i.inhseqno));

        let mut pg_constraint: Vec<_> = self.pg_constraint.values().cloned().collect();
        pg_constraint.sort_by_key(|c| c.oid);

        let mut pg_index: Vec<_> = self.pg_index.values().cloned().collect();
        pg_index.sort_by_key(|i| i.indexrelid);

        let mut pg_rewrite: Vec<_> = self.pg_rewrite.values().cloned().collect();
        pg_rewrite.sort_by_key(|r| r.oid);

        let mut pg_collation: Vec<_> = self.pg_collation.values().cloned().collect();
        pg_collation.sort_by_key(|c| c.oid);

        PgCatalogSeed {
            pg_namespace,
            pg_type,
            pg_enum,
            pg_range,
            pg_class,
            pg_attribute,
            pg_proc,
            pg_aggregate,
            pg_operator,
            pg_cast,
            pg_extension,
            pg_depend,
            pg_inherits,
            pg_constraint,
            pg_index,
            pg_rewrite,
            pg_collation,
            search_path: self.search_path.clone(),
            pg_am: self.pg_am.clone(),
            pg_opfamily: self.pg_opfamily.clone(),
            pg_opclass: self.pg_opclass.clone(),
            sql_function_defs: {
                let mut defs: Vec<_> = self
                    .sql_function_defs
                    .iter()
                    .map(|(&oid, d)| (oid, d.clone()))
                    .collect();
                defs.sort_by_key(|(oid, _)| *oid);
                defs
            },
        }
    }

    /// Record a seeded SQL function's definition and, when PG would inline
    /// it, the expression it stands for.
    fn add_sql_function_def(&mut self, oid: PgProcOid, definition: String) {
        let stmt = pg_query::parse(&definition).ok().and_then(|p| {
            match p.protobuf.stmts.first()?.stmt.as_ref()?.node.as_ref()? {
                pg_query::protobuf::node::Node::CreateFunctionStmt(s) => Some(s.clone()),
                _ => None,
            }
        });
        if let (Some(stmt), Some(proc)) = (stmt, self.pg_proc.get(&oid))
            && let Some(body) = crate::ddl::function_body::inlinable_body(&stmt, proc)
        {
            self.inline_sql_bodies.insert(oid, body);
        }
        self.sql_function_defs.insert(oid, definition);
    }

    /// Parse and apply all DDL statements in `sql`, mutating the catalog.
    pub fn apply_sql(&mut self, sql: &str) -> Result<(), DdlError> {
        let result = apply_sql_to(self, sql);
        // One `apply_sql` call is one migration, which the runner wraps in
        // its own transaction: `SET LOCAL` values end with it.
        self.end_transaction_scope();
        #[cfg(feature = "pg_sanity")]
        {
            if sql_touches_extension(sql) {
                // PG sanity doesn't ship the same extension catalog as real PG
                // (no uuid-ossp, etc.) — and even ones it does ship would
                // need explicit `--extensions=...` wiring. Once an extension
                // is in play, downstream tables/queries reference types
                // PG sanity doesn't know about, so we taint the catalog and
                // skip every later mirror call.
                self.pg_sanity_tainted = true;
            } else if !self.pg_sanity_tainted && !self.pg_sanity_skip {
                self.run_pg_sanity_apply_check(sql, &result);
            }
        }
        result
    }

    /// Parse `expr_sql` as a SELECT-list expression (`SELECT <expr>`),
    /// resolve every name slot to a catalog OID against `self`, and return
    /// the serialized AST + bindings. Used by the seed exporter to capture
    /// `pg_index.indexprs` from `pg_get_indexdef` output without requiring
    /// the seed crate to depend on the binding walker directly.
    #[cfg(any(test, feature = "internal"))]
    pub fn serialize_expression(&self, expr_sql: &str) -> Result<SerializedAst, DdlError> {
        let select = format!("SELECT {expr_sql}");
        crate::ddl::serialize_subnode(self, &select, crate::ddl::views::extract_first_target)
    }

    // ── Introspection for the differential fuzzer (type-directed generation) ──
    //
    // The fuzzer lives in the integration-test crate and can't see `pub(crate)`
    // catalog fields, so these expose just enough of the live catalog to mine
    // real functions / operators / types and build a "type → producers" index.

    /// Every `pg_proc` row (functions, aggregates, window functions, procedures).
    #[cfg(any(test, feature = "internal"))]
    pub fn iter_procs(&self) -> impl Iterator<Item = &PgProc> {
        self.pg_proc.values()
    }

    /// Every `pg_operator` row.
    #[cfg(any(test, feature = "internal"))]
    pub fn iter_operators(&self) -> impl Iterator<Item = &PgOperator> {
        self.pg_operator.values()
    }

    /// Every `pg_type` row.
    #[cfg(any(test, feature = "internal"))]
    pub fn iter_types(&self) -> impl Iterator<Item = &PgType> {
        self.pg_type.values()
    }

    /// The `pg_type` row for an OID, if present.
    #[cfg(any(test, feature = "internal"))]
    pub fn type_row(&self, oid: PgTypeOid) -> Option<&PgType> {
        self.pg_type.get(&oid)
    }

    /// User-visible relations (tables/views) with their columns, as
    /// `(relname, [(attname, atttypid, attnotnull)])`. Skips system catalogs
    /// (anything in `pg_catalog` / `information_schema`). Reuses the existing
    /// [`Self::namespace_name`] for the schema filter.
    #[cfg(any(test, feature = "internal"))]
    pub fn iter_relations(&self) -> Vec<FuzzRelation> {
        let mut out = Vec::new();
        for class in self.pg_class.values() {
            if !matches!(class.relkind, RelKind::Table | RelKind::View) {
                continue;
            }
            match self.namespace_name(class.relnamespace) {
                Some("pg_catalog") | Some("information_schema") | None => continue,
                _ => {}
            }
            let cols = self
                .pg_attribute
                .get(&class.oid)
                .map(|atts| {
                    atts.iter()
                        .filter(|a| a.attnum > 0)
                        .map(|a| (a.attname.clone(), a.atttypid, a.attnotnull))
                        .collect()
                })
                .unwrap_or_default();
            out.push((class.relname.clone(), cols));
        }
        out
    }

    /// Like [`Self::serialize_expression`] but for partial-index predicates
    /// — parses `SELECT 1 WHERE <pred>` and serializes the WHERE node.
    #[cfg(any(test, feature = "internal"))]
    pub fn serialize_predicate(&self, pred_sql: &str) -> Result<SerializedAst, DdlError> {
        let select = format!("SELECT 1 WHERE {pred_sql}");
        crate::ddl::serialize_subnode(self, &select, crate::ddl::views::extract_where)
    }

    /// Analyze a SQL query template against this catalog.
    ///
    /// Lexes `sql` to extract named parameters (`$name`), spreads (`$..name`),
    /// and nullability annotations (`$foo?`, `$foo!`); rewrites the SQL with
    /// positional placeholders; infers parameter and output column types; and
    /// returns everything combined in an [`AnalyzedQuery`].
    pub fn analyze(&self, sql: &str) -> Result<AnalyzedQuery, AnalyzeError> {
        let (_analysis_sql, result) = self.analyze_with_sql(sql);

        #[cfg(feature = "pg_sanity")]
        if !self.pg_sanity_tainted && !self.pg_sanity_skip {
            self.run_pg_sanity_analyze_check(&_analysis_sql, &result);
        }

        result
    }

    /// Inner analyze that also returns the rewritten SQL handed to the
    /// static pass — used by [`Self::analyze`] to mirror the same string on
    /// PG sanity under `pg_sanity`.
    fn analyze_with_sql(&self, sql: &str) -> (String, Result<AnalyzedQuery, AnalyzeError>) {
        // PG's wording for each lex-level failure — kept verbatim so the
        // `pg_sanity` prefix check matches. `at or near "..."` echoes a
        // snippet of the offending token; we approximate that by taking
        // ~24 bytes starting at the LexError's reported position.
        fn format_lex_error_pg(e: &crate::lexer::LexError, sql: &str) -> String {
            use crate::lexer::LexError as L;
            let near = |pos: usize| -> String {
                let bytes = sql.as_bytes();
                let mut end = (pos + 24).min(bytes.len());
                // Don't slice mid-char.
                while end < bytes.len() && (bytes[end] & 0xC0) == 0x80 {
                    end += 1;
                }
                let slice = &sql[pos..end];
                // Stop at the first newline if any — PG's `at or near`
                // typically shows a short, single-line snippet.
                let slice = slice.split('\n').next().unwrap_or(slice);
                slice.to_string()
            };
            match e {
                L::UnclosedString { position } => format!(
                    "unterminated quoted string at or near \"{}\"",
                    near(*position)
                ),
                L::UnclosedBlockComment { position } => {
                    format!("unterminated /* comment at or near \"{}\"", near(*position))
                }
                L::UnclosedDollarQuote { tag: _, position } => format!(
                    "unterminated dollar-quoted string at or near \"{}\"",
                    near(*position)
                ),
                L::UnclosedQuotedIdentifier { position } => format!(
                    "unterminated quoted identifier at or near \"{}\"",
                    near(*position)
                ),
            }
        }
        let lex_output = match lex(sql) {
            Ok(l) => l,
            Err(e) => {
                // The lexer's own errors carry a `position` in the original
                // SQL — render them with a snippet so the user sees the
                // offending location. Install a guard with an empty
                // `LexOutput` so offset translation is the identity.
                let empty_lex = crate::param::LexOutput {
                    sql: sql.to_string(),
                    params: Vec::new(),
                    spreads: Vec::new(),
                    rewrites: Vec::new(),
                };
                let _guard = crate::error::DiagContextGuard::install(sql, &empty_lex);
                let position = e.position();
                let span = crate::error::SourceSpan::one_char_at(position);
                let message = format_lex_error_pg(&e, sql);
                let raw = crate::error::RawError::lex(message, Some(span));
                return (sql.to_string(), Err(raw.finalize_implicit()));
            }
        };

        // Collect explicit nullability annotations from the lexer, ordered by
        // positional parameter index (regular params first, then spread fields).
        let mut param_nullability: Vec<Option<bool>> =
            lex_output.params.iter().map(|p| p.nullable).collect();
        for spread in &lex_output.spreads {
            if let Some(fields) = &spread.fields {
                param_nullability.extend(
                    fields
                        .iter()
                        .map(|f| if f.nullable { Some(true) } else { None }),
                );
            }
        }

        // When the query has spreads, run analysis on a sample SQL where each
        // spread is materialized as a single row of placeholders, so the
        // analyzer can infer the field types from surrounding context.
        let analysis_sql = if lex_output.spreads.is_empty() {
            lex_output.sql.clone()
        } else {
            match build_spread_sample_sql(&lex_output) {
                Ok(s) => s,
                Err(e) => return (lex_output.sql.clone(), Err(e)),
            }
        };

        // Install the diagnostic context so that error sites deep in the
        // analyzer can render snippet + caret + hint against the original
        // SQL. When the query has spreads, `analysis_sql` differs from
        // `lex_output.sql` (the lexer-rewritten form the post-lex offsets
        // refer to) — falling back to no context yields flat error messages
        // for those queries, which is acceptable until we extend the offset
        // map across spread expansion.
        let (columns, mut info_params, can_run_as_subquery) = if lex_output.spreads.is_empty() {
            let _guard = crate::error::DiagContextGuard::install(sql, &lex_output);
            match analyze_static(self, &analysis_sql, &param_nullability) {
                Ok(p) => p,
                Err(e) => return (analysis_sql, Err(e)),
            }
        } else {
            match analyze_static(self, &analysis_sql, &param_nullability) {
                Ok(p) => p,
                Err(e) => return (analysis_sql, Err(e)),
            }
        };

        // Invariant: the analyzer must produce exactly one param entry per
        // positional placeholder the lexer extracted. Surface a mismatch as
        // an Internal error so the macro host process can report it cleanly.
        let expected_param_count = lex_output.params.len()
            + lex_output
                .spreads
                .iter()
                .map(|s| s.fields.as_ref().map(|f| f.len()).unwrap_or(0))
                .sum::<usize>();
        if info_params.len() != expected_param_count {
            return (
                analysis_sql.clone(),
                Err(AnalyzeError::Internal(format!(
                    "analyzer param count ({}) does not match lexer placeholder count ({}) \
                     for SQL: {analysis_sql}",
                    info_params.len(),
                    expected_param_count,
                ))),
            );
        }

        // Merge explicit $foo? / $foo! annotations from the lexer on top of
        // the analyzer's inferred nullability (explicit always wins).
        for (pi, &lexer_nullable) in info_params.iter_mut().zip(param_nullability.iter()) {
            if let Some(explicit) = lexer_nullable {
                pi.nullable = explicit;
            }
        }

        let fused = fuse(lex_output, columns, info_params, can_run_as_subquery);
        (analysis_sql, fused)
    }

    /// Allocate a fresh user-space OID. Returns the [`NonZeroU32`] directly so
    /// callers can re-tag it as the appropriate typed OID kind via
    /// `XxxOid::from_nonzero(...)` (or the equivalent `From<NonZeroU32>`
    /// impl) without the fallible `XxxOid::new(...).expect(...)` round-trip.
    ///
    /// Errors with [`DdlError::Internal`] only when the OID space (`u32`) is
    /// exhausted — practically unreachable in a single analyzer run since
    /// allocation starts at [`USER_OID_START`] (100_000) and steps by one.
    pub(crate) fn alloc_oid(&mut self) -> Result<std::num::NonZeroU32, DdlError> {
        let oid = self.next_oid;
        self.next_oid = self
            .next_oid
            .checked_add(1)
            .ok_or_else(|| DdlError::Internal("OID space exhausted".into()))?;
        Ok(oid)
    }

    /// Disable the `pg_sanity` cross-check on this catalog instance for
    /// the rest of its lifetime. Useful when the analyzer enforces a stricter
    /// rule than what PG sanity's wire-level `prepare` validates — e.g. ON
    /// CONFLICT target matching, which real PG rejects at planning time but
    /// PG sanity's `prepare` skips. Without the feature this is a no-op so
    /// callers can always invoke it without `#[cfg]`.
    pub fn skip_pg_sanity(&mut self) {
        #[cfg(feature = "pg_sanity")]
        {
            self.pg_sanity_skip = true;
        }
    }
}

// ─── PG sanity sanity check (feature-gated) ───────────────────────────────────

/// Quick check: does `sql` contain any `CREATE/ALTER/DROP EXTENSION`
/// statement? Used to skip the PG sanity mirror when the analyzer's embedded
/// extension support diverges from what PG sanity ships natively. We parse via
/// `pg_query` (cheap — `apply_sql_to` parses anyway) and inspect the AST so
/// a column or identifier named `extension` doesn't cause a false skip.
#[cfg(feature = "pg_sanity")]
fn sql_touches_extension(sql: &str) -> bool {
    use pg_query::protobuf::node;
    let Ok(parsed) = pg_query::parse(sql) else {
        return false;
    };
    parsed.protobuf.stmts.iter().any(|raw| {
        let Some(stmt) = raw.stmt.as_ref().and_then(|n| n.node.as_ref()) else {
            return false;
        };
        match stmt {
            node::Node::CreateExtensionStmt(_) | node::Node::AlterExtensionStmt(_) => true,
            node::Node::DropStmt(d) => {
                d.remove_type == pg_query::protobuf::ObjectType::ObjectExtension as i32
            }
            _ => false,
        }
    })
}

#[cfg(feature = "pg_sanity")]
impl PgCatalog {
    /// Lazily spawn the PG sanity mirror and run `f` against it. No-op if the
    /// catalog was built via [`PgCatalog::from_seed`] (where there's no
    /// reproducible live state).
    fn with_pg_sanity<R>(
        &self,
        f: impl FnOnce(&mut crate::pg_sanity::PgSanityServer) -> R,
    ) -> Option<R> {
        let cell = self.pg_sanity.as_ref()?;
        let mutex = cell.get_or_init(|| {
            let server = crate::pg_sanity::PgSanityServer::spawn()
                .unwrap_or_else(|e| panic!("pg_sanity: {e}"));
            std::sync::Mutex::new(server)
        });
        let mut guard = mutex.lock().expect("pg_sanity: pg sanity mutex poisoned");
        Some(f(&mut guard))
    }

    fn run_pg_sanity_apply_check(&self, sql: &str, result: &Result<(), DdlError>) {
        self.with_pg_sanity::<()>(|server| server.assert_apply_matches(sql, result));
    }

    fn run_pg_sanity_analyze_check(
        &self,
        analysis_sql: &str,
        result: &Result<AnalyzedQuery, AnalyzeError>,
    ) {
        self.with_pg_sanity::<()>(|server| server.assert_analyze_matches(analysis_sql, result));
    }

    /// Analyze `sql` and cross-check the result against the embedded PG
    /// sanity mirror **without panicking** on divergence. Returns the
    /// analyzer's own result alongside an optional [`Divergence`] describing
    /// the first disagreement with PG (`None` when they agree, or when this
    /// catalog has no live mirror — e.g. built via [`PgCatalog::from_seed`]).
    ///
    /// This is the entry point the differential fuzzer drives: unlike
    /// [`PgCatalog::analyze`], it lets the caller collect many findings
    /// instead of aborting on the first. The schema stays in sync with the
    /// mirror exactly as it does for `analyze`, because both go through the
    /// same `apply_sql` path.
    pub fn analyze_checked(
        &self,
        sql: &str,
    ) -> (
        Result<AnalyzedQuery, AnalyzeError>,
        Option<crate::pg_sanity::Divergence>,
    ) {
        let (analysis_sql, result) = self.analyze_with_sql(sql);
        if self.pg_sanity_tainted || self.pg_sanity_skip {
            return (result, None);
        }
        let divergence = self
            .with_pg_sanity(|server| server.compare_analyze_matches(&analysis_sql, &result))
            .flatten();
        (result, divergence)
    }

    /// Apply DDL and cross-check against the mirror **without panicking** —
    /// the non-aborting counterpart of [`PgCatalog::apply_sql`], mirroring
    /// [`PgCatalog::analyze_checked`]. Lets the differential fuzzer drive
    /// randomized schema DDL: a clean `(Ok, None)` means analyzer and PG agree
    /// (and both now hold the new schema); any [`Divergence`] is a DDL bug.
    ///
    /// NOTE: on a divergence the analyzer's catalog and the mirror may have
    /// applied different amounts of the batch — callers that keep fuzzing
    /// should rebuild a fresh catalog rather than reuse this one.
    pub fn apply_sql_checked(
        &mut self,
        sql: &str,
    ) -> (Result<(), DdlError>, Option<crate::pg_sanity::Divergence>) {
        let result = apply_sql_to(self, sql);
        if sql_touches_extension(sql) {
            self.pg_sanity_tainted = true;
            return (result, None);
        }
        if self.pg_sanity_tainted || self.pg_sanity_skip {
            return (result, None);
        }
        let divergence = self
            .with_pg_sanity(|server| server.compare_apply_matches(sql, &result))
            .flatten();
        (result, divergence)
    }
}

// ─── DDL mutation helpers ──────────────────────────────────────────────────
//
// These keep the rows + name indexes in sync. DDL handlers should use them
// rather than writing the HashMaps directly.

impl PgCatalog {
    // Schema changes re-resolve the textual search_path: a path entry may
    // name a schema that only now exists (or no longer does).
    pub(crate) fn insert_pg_namespace(&mut self, row: PgNamespace) {
        self.namespace_by_name.insert(row.nspname.clone(), row.oid);
        self.pg_namespace.insert(row.oid, row);
        self.refresh_search_path();
    }

    pub(crate) fn remove_pg_namespace(&mut self, oid: PgNamespaceOid) -> Option<PgNamespace> {
        let row = self.pg_namespace.remove(&oid)?;
        self.namespace_by_name.remove(&row.nspname);
        self.refresh_search_path();
        Some(row)
    }

    pub(crate) fn rename_pg_namespace(&mut self, oid: PgNamespaceOid, new_name: String) {
        if let Some(row) = self.pg_namespace.get_mut(&oid) {
            self.namespace_by_name.remove(&row.nspname);
            row.nspname = new_name.clone();
            self.namespace_by_name.insert(new_name, oid);
        }
        self.refresh_search_path();
    }

    pub(crate) fn insert_pg_type(&mut self, row: PgType) {
        self.type_by_qname
            .insert((row.typnamespace, row.typname.clone()), row.oid);
        self.pg_type.insert(row.oid, row);
    }

    pub(crate) fn remove_pg_type(&mut self, oid: PgTypeOid) -> Option<PgType> {
        let row = self.pg_type.remove(&oid)?;
        self.type_by_qname
            .remove(&(row.typnamespace, row.typname.clone()));
        // Tear down dependent enum labels / range subtype / cast pairs and
        // pg_depend rows that named this type.
        self.pg_enum.remove(&oid);
        self.pg_range.remove(&oid);
        self.domain_constraints.remove(&oid);
        Some(row)
    }

    pub(crate) fn rename_pg_type(
        &mut self,
        oid: PgTypeOid,
        new_name: String,
        new_nspoid: PgNamespaceOid,
    ) {
        if let Some(row) = self.pg_type.get_mut(&oid) {
            let old_key = (row.typnamespace, row.typname.clone());
            row.typname = new_name.clone();
            row.typnamespace = new_nspoid;
            self.type_by_qname.remove(&old_key);
            self.type_by_qname.insert((new_nspoid, new_name), oid);
        }
    }

    pub(crate) fn insert_pg_class(&mut self, row: PgClass) {
        self.class_by_qname
            .insert((row.relnamespace, row.relname.clone()), row.oid);
        self.pg_class.insert(row.oid, row);
    }

    pub(crate) fn remove_pg_class(&mut self, oid: PgClassOid) -> Option<PgClass> {
        self.attr_default_types
            .retain(|(relid, _), _| *relid != oid);
        self.partition_keys.remove(&oid);
        self.triggers.remove(&oid);
        self.policies.remove(&oid);
        self.rules.remove(&oid);
        self.typed_tables.remove(&oid);
        self.clustered_indexes.remove(&oid);
        self.clustered_indexes.retain(|_, index| *index != oid);
        self.statistics.retain(|s| s.relid != oid);
        self.partition_specs.remove(&oid);
        self.partition_bounds.remove(&oid);
        let row = self.pg_class.remove(&oid)?;
        self.class_by_qname
            .remove(&(row.relnamespace, row.relname.clone()));
        self.pg_attribute.remove(&oid);
        Some(row)
    }

    pub(crate) fn rename_pg_class(
        &mut self,
        oid: PgClassOid,
        new_name: String,
        new_nspoid: PgNamespaceOid,
    ) {
        if let Some(row) = self.pg_class.get_mut(&oid) {
            let old_key = (row.relnamespace, row.relname.clone());
            row.relname = new_name.clone();
            row.relnamespace = new_nspoid;
            self.class_by_qname.remove(&old_key);
            self.class_by_qname.insert((new_nspoid, new_name), oid);
        }
    }

    pub(crate) fn insert_pg_attribute(&mut self, attr: PgAttribute) {
        let attrs = self.pg_attribute.entry(attr.attrelid).or_default();
        attrs.push(attr);
        attrs.sort_by_key(|a| a.attnum);
    }

    pub(crate) fn insert_pg_enum(&mut self, row: PgEnum) {
        let labels = self.pg_enum.entry(row.enumtypid).or_default();
        labels.push(row);
        labels.sort_by(|a, b| {
            a.enumsortorder
                .partial_cmp(&b.enumsortorder)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    pub(crate) fn insert_pg_range(&mut self, row: PgRange) {
        self.pg_range.insert(row.rngtypid, row);
    }

    pub(crate) fn insert_pg_proc(&mut self, row: PgProc) {
        self.proc_by_qname
            .entry((row.pronamespace, row.proname.clone()))
            .or_default()
            .push(row.oid);
        self.pg_proc.insert(row.oid, row);
    }

    pub(crate) fn remove_pg_proc(&mut self, oid: PgProcOid) -> Option<PgProc> {
        self.inline_sql_bodies.remove(&oid);
        self.sql_function_defs.remove(&oid);
        let row = self.pg_proc.remove(&oid)?;
        if let Some(v) = self
            .proc_by_qname
            .get_mut(&(row.pronamespace, row.proname.clone()))
        {
            v.retain(|&o| o != oid);
            if v.is_empty() {
                self.proc_by_qname
                    .remove(&(row.pronamespace, row.proname.clone()));
            }
        }
        self.pg_aggregate.remove(&oid);
        Some(row)
    }

    pub(crate) fn rename_pg_proc(
        &mut self,
        oid: PgProcOid,
        new_name: String,
        new_nspoid: PgNamespaceOid,
    ) {
        if let Some(row) = self.pg_proc.get_mut(&oid) {
            let old_key = (row.pronamespace, row.proname.clone());
            row.proname = new_name.clone();
            row.pronamespace = new_nspoid;
            if let Some(v) = self.proc_by_qname.get_mut(&old_key) {
                v.retain(|&o| o != oid);
                if v.is_empty() {
                    self.proc_by_qname.remove(&old_key);
                }
            }
            self.proc_by_qname
                .entry((new_nspoid, new_name))
                .or_default()
                .push(oid);
        }
    }

    pub(crate) fn insert_pg_aggregate(&mut self, row: PgAggregate) {
        self.pg_aggregate.insert(row.aggfnoid, row);
    }

    pub(crate) fn insert_pg_operator(&mut self, row: PgOperator) {
        self.operator_by_qname
            .entry((row.oprnamespace, row.oprname.clone()))
            .or_default()
            .push(row.oid);
        self.pg_operator.insert(row.oid, row);
    }

    pub(crate) fn remove_pg_operator(&mut self, oid: PgOperatorOid) -> Option<PgOperator> {
        let row = self.pg_operator.remove(&oid)?;
        if let Some(v) = self
            .operator_by_qname
            .get_mut(&(row.oprnamespace, row.oprname.clone()))
        {
            v.retain(|&o| o != oid);
            if v.is_empty() {
                self.operator_by_qname
                    .remove(&(row.oprnamespace, row.oprname.clone()));
            }
        }
        Some(row)
    }

    pub(crate) fn insert_pg_cast(&mut self, row: PgCast) {
        self.cast_by_pair
            .insert((row.castsource, row.casttarget), row.oid);
        self.pg_cast.insert(row.oid, row);
    }

    pub(crate) fn remove_pg_cast(&mut self, oid: PgCastOid) -> Option<PgCast> {
        let row = self.pg_cast.remove(&oid)?;
        self.cast_by_pair.remove(&(row.castsource, row.casttarget));
        Some(row)
    }

    pub(crate) fn insert_pg_extension(&mut self, row: PgExtension) {
        self.extension_by_name.insert(row.extname.clone(), row.oid);
        self.pg_extension.insert(row.oid, row);
    }

    pub(crate) fn remove_pg_extension(&mut self, oid: PgExtensionOid) -> Option<PgExtension> {
        let row = self.pg_extension.remove(&oid)?;
        self.extension_by_name.remove(&row.extname);
        Some(row)
    }

    pub(crate) fn insert_pg_constraint(&mut self, row: PgConstraint) {
        self.pg_constraint.insert(row.oid, row);
    }

    pub(crate) fn remove_pg_constraints_of(&mut self, relid: PgClassOid) {
        self.pg_constraint.retain(|_, c| c.conrelid != relid);
    }

    pub(crate) fn insert_pg_index(&mut self, row: PgIndex) {
        self.pg_index.insert(row.indexrelid, row);
    }

    pub(crate) fn remove_pg_index(&mut self, indexrelid: PgClassOid) -> Option<PgIndex> {
        self.pg_index.remove(&indexrelid)
    }

    pub(crate) fn insert_pg_rewrite(&mut self, row: PgRewrite) {
        self.pg_rewrite.insert(row.oid, row);
    }

    #[allow(dead_code)] // Used only by user-DDL `CREATE COLLATION` once implemented;
    // exists today so the test/internal feature surface is symmetric with
    // `insert_pg_*` for the other catalog tables.
    pub(crate) fn insert_pg_collation(&mut self, row: PgCollation) {
        self.collation_by_qname
            .insert((row.collnamespace, row.collname.clone()), row.oid);
        self.pg_collation.insert(row.oid, row);
    }

    /// Resolve a collation name (with optional schema) against the catalog.
    /// Walks the search path when `schema` is `None`. Returns `None` for
    /// names that don't match any registered collation — callers surface
    /// this as PG's `collation "x" does not exist` error.
    pub fn resolve_collation(&self, schema: Option<&str>, name: &str) -> Option<&PgCollation> {
        let candidate_schemas: Vec<PgNamespaceOid> = if let Some(s) = schema {
            self.namespace_oid(s).into_iter().collect()
        } else {
            let mut v = Vec::new();
            if let Some(pg_oid) = self.namespace_oid("pg_catalog")
                && !self.search_path.contains(&pg_oid)
            {
                v.push(pg_oid);
            }
            v.extend(self.search_path.iter().copied());
            v
        };
        for nsoid in candidate_schemas {
            if let Some(&oid) = self.collation_by_qname.get(&(nsoid, name.to_owned())) {
                return self.pg_collation.get(&oid);
            }
        }
        None
    }

    /// Drop every `pg_rewrite` row attached to `relid`. Used by
    /// DROP TABLE / DROP VIEW so the rule body doesn't leak past the
    /// relation.
    pub(crate) fn remove_pg_rewrites_of(&mut self, relid: PgClassOid) {
        self.pg_rewrite.retain(|_, r| r.ev_class != relid);
    }

    /// Look up the SELECT body for a view (the `_RETURN` rule). Returns
    /// `None` for non-views or if the rule was never installed.
    pub fn view_body(&self, view_oid: PgClassOid) -> Option<&SerializedAst> {
        self.pg_rewrite.values().find_map(|r| {
            (r.ev_class == view_oid && r.rulename == "_RETURN").then_some(&r.ev_action)
        })
    }

    /// Drop every `pg_index` row whose `indrelid` is `relid` and return the
    /// indexrelids — callers tear down the matching `pg_class` rows
    /// afterwards.
    pub(crate) fn remove_pg_indexes_of(&mut self, relid: PgClassOid) -> Vec<PgClassOid> {
        let to_drop: Vec<PgClassOid> = self
            .pg_index
            .values()
            .filter(|i| i.indrelid == relid)
            .map(|i| i.indexrelid)
            .collect();
        for oid in &to_drop {
            self.pg_index.remove(oid);
        }
        to_drop
    }

    /// Add a `pg_depend` row by directly inserting the `PgDepend` value the
    /// caller built. Lets call sites use struct-update syntax for the common
    /// case (only `objid`/`refobjid` change between rows, the rest is fixed
    /// for a given DDL operation).
    pub(crate) fn add_dependency(&mut self, row: PgDepend) {
        self.pg_depend.push(row);
    }

    /// Drop every `pg_depend` row whose dependent is `(classid, objid)`.
    pub(crate) fn remove_dependencies_of(&mut self, classid: PgClassOid, objid: PgGenericOid) {
        self.pg_depend
            .retain(|d| !(d.classid == classid && d.objid == objid));
    }

    /// Drop every `pg_depend` row whose referenced object is `(refclassid,
    /// refobjid)`.
    pub(crate) fn remove_dependencies_on(
        &mut self,
        refclassid: PgClassOid,
        refobjid: PgGenericOid,
    ) {
        self.pg_depend
            .retain(|d| !(d.refclassid == refclassid && d.refobjid == refobjid));
    }
}
