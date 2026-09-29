//! Row-struct and enum definitions mirroring `pg_catalog` system tables.
//!
//! Pure data definitions (the 1:1 mirror of PG catalog rows + the
//! char-mapping enums). The live [`super::PgCatalog`] container, its
//! lifecycle, and the DDL mutators live in the parent module; read-only
//! lookups in `crate::lookup`; function resolution in `crate::functions`.

use serde::{Deserialize, Serialize};
use serde_tuple::{Deserialize_tuple, Serialize_tuple};

use crate::oid::{
    PgCastOid, PgClassOid, PgCollationOid, PgConstraintOid, PgEnumOid, PgExtensionOid,
    PgGenericOid, PgNamespaceOid, PgOpclassOid, PgOperatorOid, PgProcOid, PgRewriteOid, PgTypeOid,
};

// ─── Enums ─────────────────────────────────────────────────────────────────

/// `pg_type.typtype`. PG chars: `b` base, `c` composite, `d` domain, `e` enum,
/// `p` pseudo, `r` range, `m` multirange. Serialized as the bare PG char.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypType {
    #[serde(rename = "b")]
    Base,
    #[serde(rename = "c")]
    Composite,
    #[serde(rename = "d")]
    Domain,
    #[serde(rename = "e")]
    Enum,
    #[serde(rename = "p")]
    Pseudo,
    #[serde(rename = "r")]
    Range,
    #[serde(rename = "m")]
    Multirange,
}

/// `pg_type.typstorage`: `p` plain, `e` external, `m` main, `x` extended.
/// A `p` type is not TOAST-able — its columns can only have storage PLAIN
/// and take no compression method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypStorage {
    #[serde(rename = "p")]
    Plain,
    #[serde(rename = "e")]
    External,
    #[serde(rename = "m")]
    Main,
    #[serde(rename = "x")]
    Extended,
}

/// `pg_type.typalign`: the alignment a value of the type needs when stored.
/// Serialized as the bare PG char.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypAlign {
    #[serde(rename = "c")]
    Char,
    #[serde(rename = "s")]
    Short,
    #[serde(rename = "i")]
    Int,
    #[serde(rename = "d")]
    Double,
}

impl TypAlign {
    /// The PG char (`c`, `s`, `i`, `d`).
    pub fn as_char(self) -> u8 {
        match self {
            TypAlign::Char => b'c',
            TypAlign::Short => b's',
            TypAlign::Int => b'i',
            TypAlign::Double => b'd',
        }
    }

    /// The alignment of an array, range or multirange over a type with
    /// alignment `self`: `d` stays `d`, everything else is `i` (DefineType's
    /// array, DefineRange).
    pub fn of_container(self) -> TypAlign {
        match self {
            TypAlign::Double => TypAlign::Double,
            _ => TypAlign::Int,
        }
    }
}

/// `pg_type.typcategory`. PG chars: A array, B boolean, C composite, D
/// date/time, E enum, G geometric, I network, N numeric, P pseudo, R range, S
/// string, T timespan, U user-defined, V bit-string, X unknown, Z internal.
/// Serialized as the bare PG char.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypCategory {
    #[serde(rename = "A")]
    Array,
    #[serde(rename = "B")]
    Boolean,
    #[serde(rename = "C")]
    Composite,
    #[serde(rename = "D")]
    DateTime,
    #[serde(rename = "E")]
    Enum,
    #[serde(rename = "G")]
    Geometric,
    #[serde(rename = "I")]
    Network,
    #[serde(rename = "N")]
    Numeric,
    #[serde(rename = "P")]
    Pseudo,
    #[serde(rename = "R")]
    Range,
    #[serde(rename = "S")]
    String,
    #[serde(rename = "T")]
    Timespan,
    #[serde(rename = "U")]
    UserDefined,
    #[serde(rename = "V")]
    BitString,
    #[serde(rename = "X")]
    Unknown,
    #[serde(rename = "Z")]
    Internal,
}

/// `pg_class.relkind`. Carries every variant PG emits, even the ones the
/// analyzer doesn't consult — keeping them lets the seed mirror `pg_class`
/// 1:1 without dropping rows. PG chars: `r` table, `i` index, `S` sequence,
/// `t` TOAST table, `v` view, `m` matview, `c` composite type, `f` foreign
/// table, `p` partitioned table, `I` partitioned index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RelKind {
    #[serde(rename = "r")]
    Table,
    #[serde(rename = "i")]
    Index,
    #[serde(rename = "S")]
    Sequence,
    #[serde(rename = "t")]
    ToastTable,
    #[serde(rename = "v")]
    View,
    #[serde(rename = "m")]
    MaterializedView,
    #[serde(rename = "c")]
    CompositeType,
    #[serde(rename = "f")]
    ForeignTable,
    #[serde(rename = "p")]
    Partitioned,
    #[serde(rename = "I")]
    PartitionedIndex,
}

impl RelKind {
    /// The kind of an index on a relation of kind `table`: a partitioned
    /// table's indexes are partitioned indexes.
    pub(crate) fn index_on(table: Option<RelKind>) -> RelKind {
        if table == Some(RelKind::Partitioned) {
            RelKind::PartitionedIndex
        } else {
            RelKind::Index
        }
    }

    /// The relations of this kind, as PG's errdetail_relkind_not_supported
    /// names them (`This operation is not supported for ...`).
    pub(crate) fn plural(self) -> &'static str {
        match self {
            RelKind::Table => "tables",
            RelKind::Index => "indexes",
            RelKind::Sequence => "sequences",
            RelKind::ToastTable => "TOAST tables",
            RelKind::View => "views",
            RelKind::MaterializedView => "materialized views",
            RelKind::CompositeType => "composite types",
            RelKind::ForeignTable => "foreign tables",
            RelKind::Partitioned => "partitioned tables",
            RelKind::PartitionedIndex => "partitioned indexes",
        }
    }
}

/// `pg_proc.prokind`. PG chars: `f` normal function, `a` aggregate, `w`
/// window, `p` procedure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProKind {
    #[serde(rename = "f")]
    Function,
    #[serde(rename = "a")]
    Aggregate,
    #[serde(rename = "w")]
    Window,
    #[serde(rename = "p")]
    Procedure,
}

/// `pg_proc.provolatile`. PG chars: `i` immutable (deterministic, no
/// side-effects, same input → same output forever), `s` stable (same
/// result within a single statement; depends on session/snapshot state
/// like `now()`), `v` volatile (may produce different results within one
/// statement; default for user-defined functions). The analyzer consults
/// this in DDL contexts that require IMMUTABLE callees: CHECK
/// constraints, `GENERATED ... STORED` expressions, and index
/// expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProVolatile {
    #[serde(rename = "i")]
    Immutable,
    #[serde(rename = "s")]
    Stable,
    #[serde(rename = "v")]
    Volatile,
}

/// `pg_proc.proargmodes` element. PG chars: `i` IN, `o` OUT, `b` INOUT, `v`
/// VARIADIC, `t` TABLE.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArgMode {
    #[serde(rename = "i")]
    In,
    #[serde(rename = "o")]
    Out,
    #[serde(rename = "b")]
    InOut,
    #[serde(rename = "v")]
    Variadic,
    #[serde(rename = "t")]
    Table,
}

/// `pg_attribute.attgenerated`. PG stores `\0` for "not generated", `s` for
/// STORED, `v` for VIRTUAL. The "not generated" case is modeled as
/// `Option::None` on the field, so this enum only carries the two real
/// variants. JSON form: `null` / `"s"` / `"v"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttGenerated {
    #[serde(rename = "s")]
    Stored,
    #[serde(rename = "v")]
    Virtual,
}

/// `pg_attribute.attidentity`. PG stores `\0` for "not an identity column",
/// `a` for `GENERATED ALWAYS AS IDENTITY`, `d` for `GENERATED BY DEFAULT AS
/// IDENTITY`. The "not an identity column" case is modeled as `Option::None`
/// on the field, so this enum only carries the two real variants. JSON form:
/// `null` / `"a"` / `"d"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttIdentity {
    #[serde(rename = "a")]
    Always,
    #[serde(rename = "d")]
    ByDefault,
}

/// `pg_cast.castcontext`. PG chars: `i` implicit, `a` assignment, `e`
/// explicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CastContext {
    #[serde(rename = "i")]
    Implicit,
    #[serde(rename = "a")]
    Assignment,
    #[serde(rename = "e")]
    Explicit,
}

/// `pg_cast.castmethod`. PG chars: `f` function, `b` binary, `i` I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CastMethod {
    #[serde(rename = "f")]
    Function,
    #[serde(rename = "b")]
    Binary,
    #[serde(rename = "i")]
    InOut,
}

/// `pg_rewrite.ev_type`. PG stores this as a single char: `'1'` SELECT
/// (used for views' implicit `_RETURN` rule), `'2'` UPDATE, `'3'` INSERT,
/// `'4'` DELETE.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvType {
    #[serde(rename = "1")]
    Select,
    #[serde(rename = "2")]
    Update,
    #[serde(rename = "3")]
    Insert,
    #[serde(rename = "4")]
    Delete,
}

/// `pg_rewrite.ev_enabled`. PG chars: `O` origin (default), `R` replica,
/// `A` always, `D` disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvEnabled {
    #[serde(rename = "O")]
    Origin,
    #[serde(rename = "R")]
    Replica,
    #[serde(rename = "A")]
    Always,
    #[serde(rename = "D")]
    Disabled,
}

/// `pg_depend.deptype`. PG chars used here: `n` normal, `a` auto, `i`
/// internal, `e` extension, `x` auto-extension, `p` pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DepType {
    #[serde(rename = "n")]
    Normal,
    #[serde(rename = "a")]
    Auto,
    #[serde(rename = "i")]
    Internal,
    #[serde(rename = "e")]
    Extension,
    #[serde(rename = "x")]
    AutoExtension,
    #[serde(rename = "p")]
    Pin,
}

// ─── Catalog rows ──────────────────────────────────────────────────────────

/// `pg_namespace`: a schema.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgNamespace {
    pub oid: PgNamespaceOid,
    pub nspname: String,
}

/// `pg_type`: a registered type.
///
/// Enum labels live in `pg_enum`, range subtype in `pg_range`, composite
/// fields in `pg_attribute` (keyed by `typrelid`), so this row is just
/// scalar metadata.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgType {
    pub oid: PgTypeOid,
    pub typname: String,
    /// FK `pg_namespace.oid`.
    pub typnamespace: PgNamespaceOid,
    pub typtype: TypType,
    pub typcategory: TypCategory,
    pub typispreferred: bool,
    /// FK `pg_class.oid` for composite types (the row type's class). `None`
    /// (serialized as `0`) otherwise.
    #[serde(with = "crate::oid::oid_or_zero")]
    pub typrelid: Option<PgClassOid>,
    /// FK `pg_type.oid` of the array's element type. `None` if not an array.
    #[serde(with = "crate::oid::oid_or_zero")]
    pub typelem: Option<PgTypeOid>,
    /// FK `pg_type.oid` of the canonical array type whose elements are this
    /// type. PG sets this on every base/composite/domain when it auto-creates
    /// the `_<name>` array; the legacy `oidvector` and `int2vector` have
    /// `typelem = oid/int2` but nobody points `typarray` at them, which is
    /// what disambiguates them from the real `_oid` / `_int2` arrays.
    #[serde(with = "crate::oid::oid_or_zero")]
    pub typarray: Option<PgTypeOid>,
    /// FK `pg_type.oid` of the domain's base type. `None` if not a domain.
    #[serde(with = "crate::oid::oid_or_zero")]
    pub typbasetype: Option<PgTypeOid>,
    /// Domain-level `NOT NULL`. `false` for every non-domain type. Mirrors
    /// `pg_type.typnotnull` so a column declared `colname mydomain` inherits
    /// non-nullness without an explicit column constraint.
    pub typnotnull: bool,
    /// Domain-level type modifier, mirroring `pg_type.typtypmod`. `Some`
    /// only for domains over a parametric base (`CREATE DOMAIN d AS
    /// varchar(20)`); `None` for everything else (PG's `-1`). Use
    /// [`crate::typmod`] to encode/decode.
    #[serde(with = "crate::oid::option_i32_neg_one")]
    pub typtypmod: Option<i32>,
    /// FK `pg_collation.oid` — the type's default collation. `None`
    /// (PG's `0`) for non-collatable types and base text types that
    /// inherit the database default. Set explicitly for `citext` and
    /// for domains created with an explicit `COLLATE` decoration. PG
    /// uses this to derive `attcollation` when a column doesn't pin its
    /// own collation.
    #[serde(with = "crate::oid::oid_or_zero")]
    pub typcollation: Option<PgCollationOid>,
    /// `pg_type.typstorage`.
    pub typstorage: TypStorage,
    /// `pg_type.typlen`: the fixed size of a value, `-1` for a varlena type,
    /// `-2` for a C string.
    pub typlen: i16,
    /// `pg_type.typbyval`: values are passed by value (fixed size ≤ 8).
    pub typbyval: bool,
    /// `pg_type.typalign`.
    pub typalign: TypAlign,
    /// FK `pg_proc.oid` of the subscripting handler (`array_subscript_handler`
    /// for true arrays, `jsonb_subscript_handler`, an extension's own…).
    /// `None` (PG's `0`) for types that can't be subscripted.
    #[serde(with = "crate::oid::oid_or_zero")]
    pub typsubscript: Option<PgProcOid>,
    /// `pg_type.typisdefined`: `false` for a shell type, which `CREATE TYPE
    /// name;` (or a C function returning a not-yet-defined type) creates
    /// until the full `CREATE TYPE` defines it.
    pub typisdefined: bool,
}

/// `pg_enum`: one row per enum label.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgEnum {
    pub oid: PgEnumOid,
    /// FK `pg_type.oid`.
    pub enumtypid: PgTypeOid,
    pub enumsortorder: f32,
    pub enumlabel: String,
}

/// `pg_range`: subtype info for range types.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgRange {
    /// FK `pg_type.oid`.
    pub rngtypid: PgTypeOid,
    /// FK `pg_type.oid` of the subtype.
    pub rngsubtype: PgTypeOid,
    /// FK `pg_type.oid` of the multirange type built over this range
    /// (`pg_range.rngmultitypid`). `Option` so stale seeds without the
    /// column still load; regenerate via `cargo run -p typedpg_seed`.
    #[serde(default)]
    pub rngmultitypid: Option<PgTypeOid>,
}

/// `pg_constraint.contype`. PG chars: `c` check, `f` foreign key, `n` not
/// null (PG18+), `p` primary key, `u` unique, `t` constraint trigger, `x`
/// exclusion. Constraint triggers and any future kind map to `Other` so the
/// row still round-trips through the seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConType {
    #[serde(rename = "c")]
    Check,
    #[serde(rename = "f")]
    ForeignKey,
    #[serde(rename = "p")]
    PrimaryKey,
    #[serde(rename = "u")]
    Unique,
    #[serde(rename = "x")]
    Exclusion,
    /// PG 18 not-null constraint (`conkey` = the one column). The column's
    /// `attnotnull` mirrors whether it has one.
    #[serde(rename = "n")]
    NotNull,
    /// Catch-all for `t` (constraint trigger) and any future variants.
    #[serde(other)]
    Other,
}

/// `pg_constraint`: one row per integrity constraint.
///
/// We track the dimensions the analyzer consults: the constrained relation
/// (`conrelid`), the constraint kind, the columns it covers (`conkey`,
/// 1-based attnums), the human-visible name (`conname`, used by
/// `ON CONFLICT ON CONSTRAINT name`), and the FK target (`confrelid`,
/// `confkey`) so the dependency graph is reachable in either direction.
/// FK action codes (`confdeltype`, `confupdtype`) and `consrc` are not
/// modeled.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgConstraint {
    pub oid: PgConstraintOid,
    pub conname: String,
    /// FK `pg_class.oid` of the constrained relation.
    pub conrelid: PgClassOid,
    pub contype: ConType,
    /// Attnums (1-based) of the columns covered. Empty for table-level
    /// CHECK constraints with no direct column reference.
    pub conkey: Vec<i16>,
    /// FK `pg_class.oid` of the target relation. `None` (serialized as
    /// `0`) for non-FK constraints.
    #[serde(with = "crate::oid::oid_or_zero")]
    pub confrelid: Option<PgClassOid>,
    /// Attnums (1-based) of the columns on the target relation that the
    /// FK references. Empty for non-FK constraints.
    pub confkey: Vec<i16>,
    /// The constraint is defined locally on the relation (not only
    /// inherited from a parent).
    pub conislocal: bool,
    /// Number of parents the constraint is inherited from.
    pub coninhcount: i16,
    /// The constraint is enforced (PG 18's `NOT ENFORCED` clears it; only
    /// CHECK and FOREIGN KEY constraints can be not enforced).
    pub conenforced: bool,
    /// The constraint has been validated against the existing rows (`NOT
    /// VALID` clears it until `VALIDATE CONSTRAINT`).
    pub convalidated: bool,
    /// The constraint is `NO INHERIT` (CHECK and not-null constraints).
    pub connoinherit: bool,
    /// PG 18's temporal constraints: a PRIMARY KEY / UNIQUE with `WITHOUT
    /// OVERLAPS` or a FOREIGN KEY with `PERIOD`.
    pub conperiod: bool,
}

/// `pg_index`: one row per index. Keyed by `indexrelid` — PG models the index
/// itself as a row in `pg_class` (`relkind = 'i'`) and the metadata about
/// what it's indexing here.
///
/// `indkey` mirrors PG: a list of attnums, one per indexed element. A `0`
/// means "this slot is an expression"; the expressions are walked from
/// `indexprs` in order. `indpred` carries the partial-index predicate.
/// Both expression and predicate share the [`SerializedAst`] shape so
/// dependency rewrites (RENAME / SET SCHEMA on an indexed column) flow
/// through the same applier the views use.
///
/// We don't model `indisvalid`, `indisready`, `indisclustered`, etc. — the
/// analyzer doesn't consult them and they would only inflate the seed.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgIndex {
    /// FK `pg_class.oid` of the index relation itself.
    pub indexrelid: PgClassOid,
    /// FK `pg_class.oid` of the indexed table.
    pub indrelid: PgClassOid,
    /// Total number of index columns + included columns.
    pub indnatts: i16,
    /// Number of *key* columns (excludes `INCLUDE (cols)` columns).
    pub indnkeyatts: i16,
    pub indisunique: bool,
    pub indisprimary: bool,
    /// The index backs an exclusion constraint — an `EXCLUDE` one, or PG
    /// 18's PRIMARY KEY / UNIQUE `WITHOUT OVERLAPS`.
    pub indisexclusion: bool,
    /// Attnums of the indexed columns. `0` means the slot is an expression
    /// (consumed in order from `indexprs`).
    pub indkey: Vec<i16>,
    /// Per-expression ASTs for slots where `indkey[i] == 0`, in the same
    /// order. `Vec::new()` when no slot is an expression.
    ///
    /// Diverges from PG's literal shape (PG packs the whole list into one
    /// `pg_node_tree`); splitting per expression keeps each entry's
    /// bindings stream local to its own AST and makes per-expression
    /// reanalysis trivial.
    pub indexprs: Vec<SerializedAst>,
    /// Partial-index predicate. `None` for non-partial indexes.
    pub indpred: Option<SerializedAst>,
    /// `indcollation`: each key column's collation (`None` — PG's `0` — for
    /// a non-collatable one).
    #[serde(with = "crate::oid::vec_oid_or_zero")]
    pub indcollation: Vec<Option<PgCollationOid>>,
    /// `indclass`: each key column's operator class. `None` where the
    /// analyzer couldn't resolve it (an expression of unknown type).
    #[serde(with = "crate::oid::vec_oid_or_zero")]
    pub indclass: Vec<Option<PgOpclassOid>>,
}

/// `pg_inherits`: one row per (child, parent) edge in the inheritance graph.
///
/// `CREATE TABLE child () INHERITS (p1, p2, …)` emits one row per parent.
/// `inhseqno` orders parents within a child so column merging happens
/// deterministically. The actual columns inherited from the parent live on
/// the child's own `pg_attribute` rows; this table just records the link
/// so cascade operations (`DROP COLUMN`, `RENAME`, etc.) can walk it.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgInherits {
    /// FK `pg_class.oid` — the child relation.
    pub inhrelid: PgClassOid,
    /// FK `pg_class.oid` — the parent relation.
    pub inhparent: PgClassOid,
    /// 1-based ordinal among the child's parents.
    pub inhseqno: i32,
}

/// `pg_collation`: a registered collation. PG keys this by oid; the analyzer
/// also keeps a name index so `COLLATE "x"` validation (and column-level
/// `attcollation`) can resolve names quickly.
///
/// We carry only the fields the analyzer consults: name, namespace,
/// encoding (`-1` = collation-independent / "any"). `collprovider`,
/// `collisdeterministic`, `collcollate`, `collctype`, etc. are not modeled
/// because nothing in the analyzer reads them.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgCollation {
    pub oid: PgCollationOid,
    pub collname: String,
    /// FK `pg_namespace.oid`.
    pub collnamespace: PgNamespaceOid,
    /// PG's `pg_database.encoding` integer for the collation's target
    /// encoding (`-1` = "any encoding"; common for `"C"` and `"POSIX"`).
    pub collencoding: i32,
}

/// A configuration parameter (`pg_settings`): its type, when it can be
/// set, and its valid values (`min_val` / `max_val` in `unit`,
/// `enumvals`).
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgSetting {
    pub name: String,
    /// `bool`, `integer`, `real`, `string` or `enum`.
    pub vartype: String,
    /// `internal`, `postmaster`, `sighup`, `superuser-backend`, `backend`,
    /// `superuser` or `user`.
    pub context: String,
    pub unit: String,
    pub min_val: String,
    pub max_val: String,
    pub enumvals: Vec<String>,
}

/// A text search object: `kind` `c` configuration (`pg_ts_config`), `d`
/// dictionary (`pg_ts_dict`), `p` parser (`pg_ts_parser`) or `t` template
/// (`pg_ts_template`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize_tuple, Deserialize_tuple)]
pub struct PgTsObject {
    pub kind: String,
    pub name: String,
    /// FK `pg_namespace.oid`.
    pub namespace: PgNamespaceOid,
}

/// `pg_am`: an index or table access method, by name (`amtype` `i` / `t`).
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgAm {
    pub amname: String,
    pub amtype: String,
    /// FK `pg_proc.oid` of the handler function. `None` when unknown.
    #[serde(with = "crate::oid::oid_or_zero")]
    pub amhandler: Option<PgProcOid>,
}

/// `pg_opfamily`: an operator family of an access method.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgOpfamily {
    pub opfname: String,
    /// FK `pg_namespace.oid`.
    pub opfnamespace: PgNamespaceOid,
    /// The access method's `amname`.
    pub opfmethod: String,
}

/// `pg_opclass`: an operator class — the index behavior of a type under an
/// access method; `opcdefault` ones are used when an index names none.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgOpclass {
    pub oid: PgOpclassOid,
    pub opcname: String,
    /// FK `pg_namespace.oid`.
    pub opcnamespace: PgNamespaceOid,
    /// The access method's `amname`.
    pub opcmethod: String,
    /// FK `pg_type.oid`: the input type.
    pub opcintype: PgTypeOid,
    pub opcdefault: bool,
    /// `opcfamily`: the operator family's `opfname` (of access method
    /// `opcmethod`) ...
    pub opcfamily: String,
    /// ... and its `opfnamespace`.
    pub opcfamilynamespace: PgNamespaceOid,
}

/// `pg_amop`: a search operator (`amoppurpose = 's'`) of an operator
/// family, whose identity is `(amopfamily, amopfamilynamespace,
/// amopmethod)` like [`PgOpfamily`]'s.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgAmop {
    pub amopfamily: String,
    /// FK `pg_namespace.oid` of the family.
    pub amopfamilynamespace: PgNamespaceOid,
    /// The access method's `amname`.
    pub amopmethod: String,
    /// FK `pg_type.oid`.
    pub amoplefttype: PgTypeOid,
    /// FK `pg_type.oid`.
    pub amoprighttype: PgTypeOid,
    pub amopstrategy: i16,
    /// FK `pg_operator.oid`.
    pub amopopr: PgOperatorOid,
}

/// `pg_class`: a relation (table, view, matview, partitioned table, composite
/// type's row type, sequence, index, …).
///
/// Views/matviews keep their SELECT body in `pg_rewrite` under `rulename =
/// '_RETURN'`, exactly the way PG does. Look up via [`PgCatalog::view_body`]
/// — there's no `relviewdef` shortcut on this row.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgClass {
    pub oid: PgClassOid,
    pub relname: String,
    /// FK `pg_namespace.oid`.
    pub relnamespace: PgNamespaceOid,
    pub relkind: RelKind,
    /// FK `pg_type.oid` of the row's composite type. `None` for relations
    /// that don't get one (rare in our scope).
    #[serde(with = "crate::oid::oid_or_zero")]
    pub reltype: Option<PgTypeOid>,
}

/// `pg_rewrite`: rules attached to a relation. Views are modeled as a
/// table with relkind = 'v' plus a single rule of type SELECT named
/// `_RETURN` that carries the body of the view in `ev_action`. PG also
/// stores user-defined `CREATE RULE INSTEAD INSERT/UPDATE/DELETE` rules
/// here; the analyzer doesn't currently emit those but the row shape is
/// the same.
///
/// `ev_action` is the SELECT (or DML) AST. PG stores it as `pg_node_tree`;
/// we keep it as [`SerializedAst`] (protobuf bytes + AstBindings) so
/// dependency renames flow through the same applier the views always
/// used. `ev_qual` is the rule's WHERE — `None` for views' `_RETURN`
/// rule.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgRewrite {
    pub(crate) oid: PgRewriteOid,
    pub(crate) rulename: String,
    /// FK `pg_class.oid` — the relation the rule is attached to.
    pub(crate) ev_class: PgClassOid,
    pub(crate) ev_type: EvType,
    pub(crate) ev_enabled: EvEnabled,
    pub(crate) is_instead: bool,
    /// Optional WHERE clause attached to the rule. Always `None` for
    /// views' `_RETURN`.
    pub(crate) ev_qual: Option<SerializedAst>,
    /// The rule body. For views/matviews this is the SELECT AST.
    pub(crate) ev_action: SerializedAst,
}

/// Bundle of a protobuf-encoded `typedpg_pg_query::Node` plus the per-name-slot
/// binding side-table that resolves every `RangeVar` / `ColumnRef` /
/// `FuncCall` / `TypeName` in the AST to a catalog OID.
///
/// Used by [`PgClass::relviewdef`] (the SELECT AST of a view) and — once
/// `pg_index` lands — by index expressions / predicates, where we also need
/// the AST + bindings to reanalyze after dependency renames.
///
/// The emitter and the rewrite pass walk the AST in identical pre-order:
/// each name-bearing node consumes exactly one [`AstBinding`] entry. RENAME
/// / SET SCHEMA become no-ops on the AST itself — the OIDs in the bindings
/// remain valid and the deparser looks up *current* names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize_tuple, Deserialize_tuple)]
pub struct SerializedAst {
    /// Protobuf-encoded `typedpg_pg_query::Node`. Base64 in JSON.
    #[serde(with = "serde_base64")]
    pub ast: Vec<u8>,
    /// One entry per name slot, walked in lockstep with the AST.
    pub(crate) bindings: Vec<AstBinding>,
}

/// One resolved name slot in a stored view AST.
///
/// Walked in deterministic pre-order alongside the AST: the emitter visits
/// every name-bearing node (`RangeVar` / `ColumnRef` / `FuncCall` /
/// `TypeName`) and pushes one variant; the rewrite pass walks the AST
/// identically and consumes one entry per slot. `Unresolved` is the
/// sentinel for slots that referred to a CTE / subquery alias /
/// unrecognized function — those keep the literal AST text through
/// round-trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AstBinding {
    /// A `RangeVar` or the relation-qualifier of a `ColumnRef`. JSON: `{"r":1234}`.
    #[serde(rename = "r")]
    Relation(PgClassOid),
    /// A `ColumnRef` terminal that names a real-relation column. The first
    /// payload is the relation, the second the column's `attnum`. JSON:
    /// `{"c":[1234,5]}`.
    #[serde(rename = "c")]
    Column(PgClassOid, i16),
    /// A `FuncCall.funcname`. JSON: `{"f":1234}`.
    #[serde(rename = "f")]
    Function(PgProcOid),
    /// A `TypeName` reference (CAST target, etc.). JSON: `{"t":1234}`.
    #[serde(rename = "t")]
    Type(PgTypeOid),
    /// Slot whose name doesn't resolve to a catalog object — CTE alias,
    /// subquery alias, function-FROM-item alias, unrecognized function.
    /// The rewrite pass leaves the literal AST text alone. JSON: `"_"`.
    #[serde(rename = "_")]
    Unresolved,
}

/// `pg_attribute`: one row per column of a relation (or per field of a
/// composite type — the typrelid points at a `pg_class` row in both cases).
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgAttribute {
    /// FK `pg_class.oid`.
    pub attrelid: PgClassOid,
    pub attname: String,
    /// FK `pg_type.oid`.
    pub atttypid: PgTypeOid,
    pub attnum: i16,
    pub attnotnull: bool,
    pub atthasdef: bool,
    pub attgenerated: Option<AttGenerated>,
    /// Type modifier: the `n` in `varchar(n)`, the packed `(p,s)` of
    /// `numeric(p,s)`, the dimension count of pgvector's `vector(N)`, etc.
    /// `None` matches PG's `-1` sentinel ("no typmod"); `Some` carries the
    /// packed `int32` exactly as PG would store it. Use [`crate::typmod`]
    /// to encode/decode.
    #[serde(with = "crate::oid::option_i32_neg_one")]
    pub atttypmod: Option<i32>,
    /// `pg_attribute.attidentity`. `None` for non-identity columns
    /// (PG's `\0`), `Some(Always)` for `GENERATED ALWAYS AS IDENTITY`,
    /// `Some(ByDefault)` for `GENERATED BY DEFAULT AS IDENTITY`.
    pub attidentity: Option<AttIdentity>,
    /// FK `pg_collation.oid` — the column's default collation. PG sets
    /// this for collatable types (text, varchar, citext, …) and 0 for
    /// non-collatable types. We model both as `None`/`Some` and let the
    /// DDL parser assign explicit `COLLATE "x"` decorations.
    #[serde(with = "crate::oid::oid_or_zero")]
    pub attcollation: Option<PgCollationOid>,
    /// The column is defined locally (not only inherited from a parent).
    pub attislocal: bool,
    /// Number of parents (inheritance or partitioning) the column is
    /// inherited from; such a column can't be dropped, renamed or retyped on
    /// its own.
    pub attinhcount: i16,
}

/// `pg_proc`: a function, aggregate, window function, or procedure.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgProc {
    pub oid: PgProcOid,
    pub proname: String,
    /// FK `pg_namespace.oid`.
    pub pronamespace: PgNamespaceOid,
    pub prokind: ProKind,
    /// Argument types in call-signature order. Excludes pure-OUT and TABLE
    /// args (those only appear in `proallargtypes`).
    pub proargtypes: Vec<PgTypeOid>,
    /// FK `pg_type.oid`.
    pub prorettype: PgTypeOid,
    pub proretset: bool,
    /// FK `pg_type.oid` of the variadic element type, or `None` if not
    /// variadic.
    #[serde(with = "crate::oid::oid_or_zero")]
    pub provariadic: Option<PgTypeOid>,
    pub proisstrict: bool,
    pub pronargdefaults: i16,
    /// All formal arg types (IN/OUT/INOUT/VARIADIC/TABLE), parallel to
    /// `proargmodes`/`proargnames`. Empty when no OUT/INOUT/TABLE args.
    pub proallargtypes: Vec<PgTypeOid>,
    pub proargmodes: Vec<ArgMode>,
    pub proargnames: Vec<String>,
    /// Volatility category. Drives DDL validation that requires IMMUTABLE
    /// callees (CHECK / GENERATED / index expressions).
    pub provolatile: ProVolatile,
    /// Type of each `proargdefaults` expression, parallel to the trailing
    /// `pronargdefaults` input parameters (`exprType` of the stored
    /// default). A default of a non-polymorphic parameter is coerced to the
    /// parameter's type at CREATE FUNCTION, so only a polymorphic
    /// parameter's default can differ — and its type takes part in
    /// resolving the call's polymorphic types (`ParseFuncOrColumn`).
    pub proargdefaulttypes: Vec<PgTypeOid>,
}

/// `pg_aggregate`: extra metadata for aggregate `pg_proc` rows.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgAggregate {
    /// FK `pg_proc.oid` — the aggregate's own pg_proc entry.
    pub aggfnoid: PgProcOid,
    /// FK `pg_proc.oid` of the final function. `None` (PG: `0`) when the
    /// aggregate has no finalfn, in which case its effective return type
    /// is `aggfnoid.prorettype`. Callers walk the FK to derive the
    /// effective return type instead of caching it on the row.
    #[serde(with = "crate::oid::oid_or_zero")]
    pub aggfinalfn: Option<PgProcOid>,
    /// Plain, ordered-set (`WITHIN GROUP`) or hypothetical-set aggregate.
    pub aggkind: AggKind,
    /// Number of direct (non-`WITHIN GROUP`) arguments of an ordered-set
    /// aggregate — a variadic one counts as one; 0 for plain aggregates.
    pub aggnumdirectargs: i16,
}

/// `pg_aggregate.aggkind`. PG chars: `n` normal, `o` ordered-set, `h`
/// hypothetical-set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AggKind {
    #[serde(rename = "n")]
    Normal,
    #[serde(rename = "o")]
    OrderedSet,
    #[serde(rename = "h")]
    Hypothetical,
}

/// `pg_operator`: a registered operator.
///
/// `oprresult` is `Option<PgTypeOid>` — PG `0` is a "shell operator" placed
/// by `CREATE OPERATOR` before the implementation function exists. Such
/// operators can't be used in queries; the operator-resolution path skips
/// them when their result type is `None`. We still keep the row so the
/// catalog mirror is faithful and a follow-up CREATE OPERATOR can finish
/// it.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgOperator {
    pub oid: PgOperatorOid,
    pub oprname: String,
    /// FK `pg_namespace.oid`.
    pub oprnamespace: PgNamespaceOid,
    /// FK `pg_type.oid`. `None` for prefix operators.
    #[serde(with = "crate::oid::oid_or_zero")]
    pub oprleft: Option<PgTypeOid>,
    /// FK `pg_type.oid`.
    pub oprright: PgTypeOid,
    /// FK `pg_type.oid`. `None` for shell operators (PG: `0`).
    #[serde(with = "crate::oid::oid_or_zero")]
    pub oprresult: Option<PgTypeOid>,
    /// FK `pg_proc.oid` of the implementing function. `None` for shell
    /// operators (PG: `0`).
    #[serde(with = "crate::oid::oid_or_zero")]
    pub oprcode: Option<PgProcOid>,
    /// FK `pg_operator.oid` of the commutator. `None` when it has none
    /// (PG: `0`).
    #[serde(with = "crate::oid::oid_or_zero")]
    pub oprcom: Option<PgOperatorOid>,
}

/// `pg_cast`: a cast rule between two types.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgCast {
    pub oid: PgCastOid,
    /// FK `pg_type.oid`.
    pub castsource: PgTypeOid,
    /// FK `pg_type.oid`.
    pub casttarget: PgTypeOid,
    pub castcontext: CastContext,
    pub castmethod: CastMethod,
    /// FK `pg_proc.oid` of the cast function (`castmethod = 'f'`); `None`
    /// for binary / I/O casts (PG: `0`).
    #[serde(with = "crate::oid::oid_or_zero")]
    pub castfunc: Option<PgProcOid>,
}

/// `pg_extension`: an installed extension.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgExtension {
    pub oid: PgExtensionOid,
    pub extname: String,
    /// FK `pg_namespace.oid`.
    pub extnamespace: PgNamespaceOid,
    pub extversion: String,
}

/// `pg_depend`: a dependency between two catalog objects.
///
/// `(classid, objid, objsubid)` identifies the dependent; `(refclassid,
/// refobjid, refobjsubid)` identifies the referenced object. `deptype` tells
/// the rule:
/// - `Normal` — view depends on a table/column; DROP CASCADE drops the view.
/// - `Extension` — `objid` was created by `refobjid` (extension); DROP
///   EXTENSION drops the object.
/// - Other variants mirror PG semantics but are not produced by the DDL
///   handlers today (kept for round-trip with PG snapshots).
///
/// `objid`/`refobjid` use [`PgGenericOid`] (a non-zero `u32`) because their
/// concrete catalog table varies with the sibling `classid`/`refclassid` —
/// callers convert to/from the per-table newtype at the boundary via
/// `PgXxxOid::new(g.get())`.
#[derive(Debug, Clone, Serialize_tuple, Deserialize_tuple)]
pub struct PgDepend {
    pub classid: PgClassOid,
    pub objid: PgGenericOid,
    pub objsubid: i16,
    pub refclassid: PgClassOid,
    pub refobjid: PgGenericOid,
    pub refobjsubid: i16,
    pub deptype: DepType,
}

// ─── Base64 adapter for relviewdef ─────────────────────────────────────────

/// Serde adapter that encodes `Vec<u8>` as a base64 string in JSON while
/// keeping it as a plain byte buffer in memory. Used by
/// [`PgClass::relviewdef`] so view ASTs don't blow up the seed JSON.
mod serde_base64 {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(de)?;
        STANDARD.decode(&s).map_err(serde::de::Error::custom)
    }
}
