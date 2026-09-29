//! Read-only queries and lookup helpers for [`PgCatalog`].
//!
//! Hosts the second `impl PgCatalog` block — everything that reads the
//! catalog rows and indexes without mutating them. Includes the PG §10.2
//! operator resolution algorithm, name/OID resolution along the search path,
//! and helpers like [`PgCatalog::attributes_of`] and
//! [`PgCatalog::enum_labels_of`] that consumers use instead of poking at the
//! HashMaps directly.

use crate::oid::{PgClassOid, PgExtensionOid, PgNamespaceOid, PgTypeOid};
use crate::pg_catalog::{
    CastContext, CastMethod, DepType, PG_CLASS_RELID, PG_EXTENSION_RELID, PG_TYPE_RELID,
    PgAttribute, PgCast, PgCatalog, PgClass, PgDepend, PgOperator, PgProc, PgType, TypType, oid,
};

/// Result of operator resolution: operand and result OIDs with any
/// polymorphic pseudo-types (`anyelement`, `anycompatiblearray`, …) already
/// substituted by the concrete types of the arguments.
#[derive(Debug, Clone)]
pub struct ResolvedOperator {
    pub left_type_oid: Option<PgTypeOid>,
    /// The operator's declared left operand (`oprleft`), before polymorphic
    /// resolution. For an ordering operator this is its btree opclass's
    /// input type (`opcintype`): `text` for a `varchar` key, `anyarray` for
    /// an array.
    pub declared_left_type_oid: Option<PgTypeOid>,
    pub right_type_oid: PgTypeOid,
    pub result_type_oid: PgTypeOid,
    /// The implementing function (`oprcode`).
    pub code: Option<crate::oid::PgProcOid>,
    /// The operator chosen (`pg_operator.oid`).
    pub oid: crate::oid::PgOperatorOid,
}

/// Outcome of [`PgCatalog::find_operator_detailed`]: a unique winner, no
/// viable candidate, or several candidates left tied after every tiebreak
/// (PG reports the last as `operator is not unique`, SQLSTATE 42725), or an
/// error of its own — a missing schema in `OPERATOR(s.op)`, or polymorphic
/// operands that can't be resolved (`anyelement = unknown` on both sides).
pub enum OperatorMatch {
    Found(ResolvedOperator),
    NotFound,
    Ambiguous,
    Error(crate::error::AnalyzeError),
}

const PG_CATALOG_SCHEMA: &str = "pg_catalog";

impl PgCatalog {
    // ── Namespace helpers ───────────────────────────────────────────────

    pub fn namespace_oid(&self, name: &str) -> Option<PgNamespaceOid> {
        // `pg_temp` names the session's temporary schema, which only the
        // migrations' session has.
        let temp = self.temp_namespace.filter(|_| self.in_migration);
        if name == "pg_temp" {
            return temp;
        }
        let oid = self.namespace_by_name.get(name).copied()?;
        if Some(oid) == self.temp_namespace && temp.is_none() {
            return None;
        }
        Some(oid)
    }

    pub fn namespace_name(&self, oid: PgNamespaceOid) -> Option<&str> {
        self.pg_namespace.get(&oid).map(|n| n.nspname.as_str())
    }

    /// IsSystemClass (catalog.c): a relation with a pinned OID — below
    /// FirstUnpinnedObjectId (12000): the system catalogs, their indexes and
    /// TOAST tables — or one in a TOAST schema. Even a superuser may not
    /// alter or drop these (without allow_system_table_mods).
    pub(crate) fn is_system_class(&self, relid: PgClassOid) -> bool {
        /// FirstUnpinnedObjectId (transam.h).
        const FIRST_UNPINNED_OBJECT_ID: u32 = 12000;
        relid.get() < FIRST_UNPINNED_OBJECT_ID
            || self.pg_class.get(&relid).is_some_and(|c| {
                self.namespace_name(c.relnamespace)
                    .is_some_and(|n| n == "pg_toast" || n.starts_with("pg_toast_temp_"))
            })
    }

    /// IsCatalogNamespace / IsToastNamespace (catalog.c): relations may not
    /// be created in `pg_catalog` or a TOAST schema (heap_create).
    pub(crate) fn is_system_namespace(&self, nsoid: PgNamespaceOid) -> bool {
        self.namespace_name(nsoid).is_some_and(|n| {
            n == PG_CATALOG_SCHEMA || n == "pg_toast" || n.starts_with("pg_toast_temp_")
        })
    }

    /// OID of the `pg_catalog` schema (looked up once per call). Returns
    /// `None` only on an empty catalog.
    pub(crate) fn pg_catalog_oid(&self) -> Option<PgNamespaceOid> {
        self.namespace_oid(PG_CATALOG_SCHEMA)
    }

    /// `true` if `pg_catalog` appears explicitly on the search path.
    fn search_path_includes_pg_catalog(&self) -> bool {
        match self.pg_catalog_oid() {
            Some(oid) => self.search_path.contains(&oid),
            None => false,
        }
    }

    /// Search-path-aware schema resolution: if `schema` is `Some`, returns
    /// `[that_oid]`; otherwise returns the search path with `pg_catalog`
    /// implicitly prepended (PG §5.9.5).
    pub(crate) fn schemas_for_lookup(&self, schema: Option<&str>) -> Vec<PgNamespaceOid> {
        if let Some(name) = schema {
            return self.namespace_oid(name).into_iter().collect();
        }
        let mut out = Vec::with_capacity(self.search_path.len() + 2);
        // The temporary schema is searched first (recomputeNamespacePath),
        // unless the path lists `pg_temp` somewhere.
        if let Some(temp) = self.temp_namespace.filter(|_| self.in_migration)
            && !self.search_path.contains(&temp)
        {
            out.push(temp);
        }
        if !self.search_path_includes_pg_catalog()
            && let Some(pg_oid) = self.pg_catalog_oid()
        {
            out.push(pg_oid);
        }
        out.extend(self.search_path.iter().copied());
        out
    }

    // ── Relation / type / function lookups ──────────────────────────────

    /// Look up a relation by name, walking the search path when `schema` is
    /// `None`. Mirrors PG §5.9.5 (pg_catalog implicitly searched first).
    pub fn resolve_table(&self, schema: Option<&str>, name: &str) -> Option<&PgClass> {
        for nsoid in self.schemas_for_lookup(schema) {
            if let Some(&class_oid) = self.class_by_qname.get(&(nsoid, name.to_owned()))
                && let Some(class) = self.pg_class.get(&class_oid)
            {
                return Some(class);
            }
        }
        None
    }

    /// Iterate over `relname`s for tables/views/sequences visible in
    /// `schema` (or in the search path when `schema` is `None`). Used to
    /// produce "did you mean ..." hints for `UndefinedTable`.
    pub(crate) fn visible_relnames<'a>(
        &'a self,
        schema: Option<&'a str>,
    ) -> impl Iterator<Item = &'a str> + 'a {
        self.schemas_for_lookup(schema)
            .into_iter()
            .flat_map(move |nsoid| {
                self.class_by_qname
                    .iter()
                    .filter(move |((ns, _), _)| *ns == nsoid)
                    .map(|((_, name), _)| name.as_str())
            })
    }

    /// Look up a type by name, walking the search path when `schema` is
    /// `None`.
    pub fn resolve_type_by_name(&self, schema: Option<&str>, name: &str) -> Option<&PgType> {
        for nsoid in self.schemas_for_lookup(schema) {
            if let Some(&type_oid) = self.type_by_qname.get(&(nsoid, name.to_owned()))
                && let Some(t) = self.pg_type.get(&type_oid)
            {
                return Some(t);
            }
        }
        None
    }

    /// Look up a type by OID.
    pub fn get_type(&self, oid: PgTypeOid) -> Option<&PgType> {
        self.pg_type.get(&oid)
    }

    /// Find the OID of the array type whose elements are `element_oid`.
    /// Resolves through `pg_type.typarray`, which PG keeps pointing at the
    /// canonical `_<name>` array; legacy types like `oidvector` /
    /// `int2vector` share `typelem` with `oid`/`int2` but are not pointed
    /// to by anyone's `typarray`, so they're correctly excluded.
    pub(crate) fn array_type_of(&self, element_oid: PgTypeOid) -> Option<PgTypeOid> {
        self.pg_type.get(&element_oid).and_then(|t| t.typarray)
    }

    /// Unwrap domains to their base type OID (capped at 32 levels to avoid
    /// pathological cycles in malformed catalogs).
    pub(crate) fn unwrap_domain(&self, oid: PgTypeOid) -> PgTypeOid {
        let mut current = oid;
        for _ in 0..32 {
            match self.pg_type.get(&current) {
                Some(t) if t.typtype == TypType::Domain => match t.typbasetype {
                    Some(base) => current = base,
                    None => break,
                },
                _ => break,
            }
        }
        current
    }

    /// Walk the domain chain looking for a `typnotnull` row. Returns the
    /// `typname` of the first domain that forbids NULLs, or `None` if no
    /// domain in the chain has the constraint. Capped at 32 hops, same as
    /// [`unwrap_domain`], to stay safe against malformed catalogs.
    pub(crate) fn domain_not_null_name(&self, oid: PgTypeOid) -> Option<&str> {
        let mut current = oid;
        for _ in 0..32 {
            let t = self.pg_type.get(&current)?;
            if t.typtype == TypType::Domain && t.typnotnull {
                return Some(&t.typname);
            }
            if t.typtype == TypType::Domain {
                current = t.typbasetype?;
            } else {
                break;
            }
        }
        None
    }

    /// True when the type chain forces non-nullable semantics on the column,
    /// independent of `pg_attribute.attnotnull`.
    pub(crate) fn type_is_not_null(&self, oid: PgTypeOid) -> bool {
        self.domain_not_null_name(oid).is_some()
    }

    /// Resolve the modifier that should apply to a column, given its
    /// `pg_attribute.atttypmod` and the column's type. PG semantics:
    /// `atttypmod` wins when present; otherwise we walk the domain chain
    /// looking for a `typtypmod` to inherit. This way `CREATE DOMAIN d AS
    /// varchar(20); CREATE TABLE t (x d)` produces a column with the right
    /// length even though `parse_column_def` left `atttypmod = None`.
    pub(crate) fn effective_typmod(&self, oid: PgTypeOid, atttypmod: Option<i32>) -> Option<i32> {
        if atttypmod.is_some() {
            return atttypmod;
        }
        let mut current = oid;
        for _ in 0..32 {
            let t = self.pg_type.get(&current)?;
            if let Some(v) = t.typtypmod {
                return Some(v);
            }
            if t.typtype == TypType::Domain {
                current = t.typbasetype?;
            } else {
                break;
            }
        }
        None
    }

    /// Subtype of a range type (`pg_range.rngsubtype`): `tstzrange` →
    /// `timestamptz`. `None` when `oid` is not a range type.
    pub(crate) fn range_subtype(&self, range_oid: PgTypeOid) -> Option<PgTypeOid> {
        self.pg_range.get(&range_oid).map(|r| r.rngsubtype)
    }

    /// The multirange type built over a range type
    /// (`pg_range.rngmultitypid`): `tstzrange` → `tstzmultirange`. `None`
    /// for non-range types and for user-defined ranges created by the DDL
    /// interpreter (which doesn't build companion multiranges yet).
    pub(crate) fn multirange_of_range(&self, range_oid: PgTypeOid) -> Option<PgTypeOid> {
        self.pg_range.get(&range_oid).and_then(|r| r.rngmultitypid)
    }

    /// The range type a multirange is built over (reverse of
    /// [`Self::multirange_of_range`]; linear scan — `pg_range` has a few
    /// dozen rows).
    pub(crate) fn range_of_multirange(&self, multirange_oid: PgTypeOid) -> Option<PgTypeOid> {
        self.pg_range
            .values()
            .find(|r| r.rngmultitypid == Some(multirange_oid))
            .map(|r| r.rngtypid)
    }

    /// Check if an implicit cast exists from `source` to `target`.
    pub fn has_implicit_cast(&self, source: PgTypeOid, target: PgTypeOid) -> bool {
        if source == target {
            return true;
        }
        match self.cast_by_pair.get(&(source, target)) {
            Some(&oid) => matches!(
                self.pg_cast.get(&oid),
                Some(PgCast {
                    castcontext: CastContext::Implicit,
                    ..
                })
            ),
            None => false,
        }
    }

    /// Check if `source` is binary-coercible to `target` — the PG rule that
    /// lets `ALTER COLUMN TYPE` skip a table rewrite.
    ///
    /// True when:
    /// - `source == target`
    /// - `source` is a domain whose base type is `target` (one level)
    /// - `pg_cast` has an implicit, binary-method entry from `source` to `target`
    pub fn is_binary_coercible(&self, source: PgTypeOid, target: PgTypeOid) -> bool {
        if source == target {
            return true;
        }
        if let Some(t) = self.pg_type.get(&source)
            && t.typtype == TypType::Domain
            && t.typbasetype == Some(target)
        {
            return true;
        }
        match self.cast_by_pair.get(&(source, target)) {
            Some(&oid) => matches!(
                self.pg_cast.get(&oid),
                Some(PgCast {
                    castcontext: CastContext::Implicit,
                    castmethod: CastMethod::Binary,
                    ..
                })
            ),
            None => false,
        }
    }

    /// Iterate over function names visible from `schema` (or the full
    /// search path when `schema` is `None`). Used for `did you mean` hints
    /// on `UndefinedFunction`.
    /// Iterate over type names visible from `schema` (or the search path
    /// when `schema` is `None`). Used for `did you mean` hints on
    /// `UndefinedType`.
    pub(crate) fn visible_type_names<'a>(
        &'a self,
        schema: Option<&'a str>,
    ) -> impl Iterator<Item = &'a str> + 'a {
        self.schemas_for_lookup(schema)
            .into_iter()
            .flat_map(move |nsoid| {
                self.type_by_qname
                    .iter()
                    .filter(move |((ns, _), _)| *ns == nsoid)
                    .map(|((_, name), _)| name.as_str())
            })
    }

    pub(crate) fn visible_function_names<'a>(
        &'a self,
        schema: Option<&'a str>,
    ) -> impl Iterator<Item = &'a str> + 'a {
        self.schemas_for_lookup(schema)
            .into_iter()
            .flat_map(move |nsoid| {
                self.proc_by_qname
                    .iter()
                    .filter(move |((ns, _), _)| *ns == nsoid)
                    .map(|((_, name), _)| name.as_str())
            })
    }

    /// Find all functions matching a name, walking the search path when
    /// `schema` is `None`.
    ///
    /// When `schema` is `Some`, only overloads in that schema are returned.
    /// When `None`, overloads from every schema on the search_path (plus
    /// `pg_catalog` if not explicitly listed) are concatenated.
    pub fn find_functions(&self, schema: Option<&str>, name: &str) -> Vec<&PgProc> {
        let mut out = Vec::new();
        for nsoid in self.schemas_for_lookup(schema) {
            if let Some(oids) = self.proc_by_qname.get(&(nsoid, name.to_owned())) {
                for &oid in oids {
                    if let Some(p) = self.pg_proc.get(&oid) {
                        out.push(p);
                    }
                }
            }
        }
        out
    }

    /// Find an operator matching name and operand types (see
    /// [`Self::find_operator_detailed`]); `None` for every failure.
    pub fn find_operator(
        &self,
        name: &str,
        left_oid: Option<PgTypeOid>,
        right_oid: PgTypeOid,
    ) -> Option<ResolvedOperator> {
        match self.find_operator_detailed(name, left_oid, right_oid) {
            OperatorMatch::Found(op) => Some(op),
            _ => None,
        }
    }

    /// PG's operator resolution (`oper` / `left_oper` + `make_op`,
    /// parse_oper.c): an exact match first — for `T op unknown` also
    /// `T op T`, then the domain's base type — else the candidates the
    /// operands implicitly coerce to, narrowed by the same heuristics as
    /// functions ([`crate::functions::func_select_candidate`]). The winner's
    /// polymorphic operands are then resolved
    /// (`enforce_generic_type_consistency`), which may fail on its own.
    ///
    /// `name` may be schema-qualified (`pg_catalog.+`, from `OPERATOR(…)`
    /// syntax — operator names never contain a dot); `left_oid = None` is
    /// a prefix operator. Distinguishes "no candidate at all" from "several
    /// candidates survived every tiebreak" — PG reports the latter as
    /// `operator is not unique: …` (SQLSTATE 42725).
    pub(crate) fn find_operator_detailed(
        &self,
        name: &str,
        left_oid: Option<PgTypeOid>,
        right_oid: PgTypeOid,
    ) -> OperatorMatch {
        let (schema, opname) = match name.rsplit_once('.') {
            Some((s, o)) => (Some(s), o),
            None => (None, name),
        };
        if let Some(s) = schema
            && self.namespace_oid(s).is_none()
        {
            return OperatorMatch::Error(
                crate::pgmsg::schema_does_not_exist(s, None).finalize_implicit(),
            );
        }
        let candidates = self.operator_candidates(schema, opname, left_oid.is_none());
        if candidates.is_empty() {
            return OperatorMatch::NotFound;
        }
        let args_of = |o: &PgOperator| -> Vec<PgTypeOid> {
            o.oprleft.into_iter().chain([o.oprright]).collect()
        };
        let actuals: Vec<PgTypeOid> = left_oid.into_iter().chain([right_oid]).collect();

        let exact = |l: Option<PgTypeOid>, r: PgTypeOid| {
            candidates
                .iter()
                .copied()
                .find(|o| o.oprleft == l && o.oprright == r)
        };
        let chosen = match left_oid {
            // `binary_oper_exact`: an unknown side is assumed to be the
            // other side's type; failing that, its base type.
            Some(l) => {
                let (el, er, was_unknown) = match (l, right_oid) {
                    (oid::UNKNOWN, r) => (r, r, true),
                    (l, oid::UNKNOWN) => (l, l, true),
                    (l, r) => (l, r, false),
                };
                exact(Some(el), er).or_else(|| {
                    let b = self.unwrap_domain(el);
                    (was_unknown && b != el)
                        .then(|| exact(Some(b), b))
                        .flatten()
                })
            }
            None => exact(None, right_oid),
        };
        let chosen = match chosen {
            Some(op) => op,
            None => {
                // `oper_select_candidate`.
                let arg_lists: Vec<Vec<PgTypeOid>> =
                    candidates.iter().map(|o| args_of(o)).collect();
                let arg_refs: Vec<&[PgTypeOid]> = arg_lists.iter().map(Vec::as_slice).collect();
                let matching = crate::functions::func_match_argtypes(&actuals, &arg_refs, self);
                match matching.as_slice() {
                    [] => return OperatorMatch::NotFound,
                    [one] => candidates[*one],
                    _ => {
                        let narrowed: Vec<&[PgTypeOid]> =
                            matching.iter().map(|&i| arg_refs[i]).collect();
                        match crate::functions::func_select_candidate(&actuals, &narrowed, self) {
                            Some(j) => candidates[matching[j]],
                            None => return OperatorMatch::Ambiguous,
                        }
                    }
                }
            }
        };

        // `make_op`: a shell operator (made by a COMMUTATOR / NEGATOR
        // reference) has no implementation yet.
        let mut declared = args_of(chosen);
        let (Some(result), Some(_)) = (chosen.oprresult, chosen.oprcode) else {
            let shown = |t: PgTypeOid| crate::ddl::util::format_type_for_message(self, t);
            return OperatorMatch::Error(
                crate::pgmsg::operator_is_only_a_shell(
                    chosen.oprleft.map(shown).as_deref(),
                    opname,
                    &shown(chosen.oprright),
                )
                .finalize_implicit(),
            );
        };
        match crate::polymorphic::enforce_generic_type_consistency(
            &actuals,
            &mut declared,
            result,
            self,
        ) {
            Ok((result_type_oid, _)) => OperatorMatch::Found(ResolvedOperator {
                left_type_oid: chosen.oprleft.map(|_| declared[0]),
                declared_left_type_oid: chosen.oprleft,
                right_type_oid: *declared.last().unwrap_or(&chosen.oprright),
                result_type_oid,
                code: chosen.oprcode,
                oid: chosen.oid,
            }),
            Err(e) => OperatorMatch::Error(e),
        }
    }

    /// PG's `OpernameGetCandidates`: the operators named `name` of the
    /// requested kind (prefix or binary) in `schema`, or along the search
    /// path (plus `pg_catalog` when not listed explicitly). An operator
    /// whose operand types repeat one from a schema earlier on the path is
    /// hidden by it. Shell operators (implementation not linked yet) are
    /// candidates too; choosing one is an error (`make_op`).
    fn operator_candidates(
        &self,
        schema: Option<&str>,
        name: &str,
        prefix: bool,
    ) -> Vec<&PgOperator> {
        let mut out: Vec<&PgOperator> = Vec::new();
        for nsoid in self.schemas_for_lookup(schema) {
            let Some(oids) = self.operator_by_qname.get(&(nsoid, name.to_owned())) else {
                continue;
            };
            let first_of_schema = out.len();
            for &oid in oids {
                if let Some(op) = self.pg_operator.get(&oid)
                    && op.oprleft.is_none() == prefix
                    && !out[..first_of_schema]
                        .iter()
                        .any(|p| p.oprleft == op.oprleft && p.oprright == op.oprright)
                {
                    out.push(op);
                }
            }
        }
        out
    }

    // ── Relationship helpers ────────────────────────────────────────────

    /// All attributes of a relation (table/view/composite type), ordered by
    /// `attnum`. Returns an empty slice when the relation has none (or when
    /// `relid` is unknown — callers usually verify the relation first).
    pub fn attributes_of(&self, relid: PgClassOid) -> &[PgAttribute] {
        self.pg_attribute
            .get(&relid)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Find one attribute of a relation by name. Linear scan over the
    /// relation's attributes (typically a handful).
    pub(crate) fn attribute_by_name(&self, relid: PgClassOid, name: &str) -> Option<&PgAttribute> {
        self.attributes_of(relid).iter().find(|a| a.attname == name)
    }

    /// Enum labels of a type, ordered by `enumsortorder`.
    pub fn enum_labels_of(&self, typid: PgTypeOid) -> Vec<&str> {
        self.pg_enum
            .get(&typid)
            .map(|v| v.iter().map(|e| e.enumlabel.as_str()).collect())
            .unwrap_or_default()
    }

    /// Composite type fields (the `pg_attribute` rows of the type's
    /// `pg_class` row, found via `typrelid`).
    pub fn composite_fields_of(&self, typid: PgTypeOid) -> &[PgAttribute] {
        let Some(t) = self.pg_type.get(&typid) else {
            return &[];
        };
        let Some(relid) = t.typrelid else {
            return &[];
        };
        self.attributes_of(relid)
    }

    /// All `pg_class` rows a view depends on (deptype=Normal, classid=
    /// PG_CLASS_RELID). Yields `(refobjid, refobjsubid)` — `refobjsubid` is
    /// the column attnum, or 0 if the view depends on the whole relation.
    pub fn view_dependencies(
        &self,
        view_oid: PgClassOid,
    ) -> impl Iterator<Item = (PgClassOid, i16)> + '_ {
        self.pg_depend
            .iter()
            .filter(move |d| {
                d.classid == PG_CLASS_RELID
                    && d.objid.get() == view_oid.get()
                    && d.refclassid == PG_CLASS_RELID
                    && matches!(d.deptype, DepType::Normal)
            })
            .map(|d| {
                (
                    PgClassOid::from_nonzero(d.refobjid.into_nonzero()),
                    d.refobjsubid,
                )
            })
    }

    /// All catalog objects an extension created (deptype=Extension,
    /// refclassid=PG_EXTENSION_RELID). Yields `(classid, objid)`.
    pub(crate) fn extension_objects(
        &self,
        ext_oid: PgExtensionOid,
    ) -> impl Iterator<Item = (PgClassOid, crate::oid::PgGenericOid)> + '_ {
        self.pg_depend.iter().filter_map(move |d| {
            (d.refclassid == PG_EXTENSION_RELID
                && d.refobjid.get() == ext_oid.get()
                && matches!(d.deptype, DepType::Extension))
            .then_some((d.classid, d.objid))
        })
    }

    /// Name of the extension that owns this type, if any. Looks for a
    /// `pg_depend` row with `deptype=Extension`, `classid=PG_TYPE_RELID`,
    /// `objid=type_oid`, and resolves `refobjid` to the extension's `extname`.
    pub fn extension_of_type(&self, type_oid: PgTypeOid) -> Option<&str> {
        for d in &self.pg_depend {
            if matches!(d.deptype, DepType::Extension)
                && d.classid == PG_TYPE_RELID
                && d.objid.get() == type_oid.get()
                && d.refclassid == PG_EXTENSION_RELID
                && let Some(ext_oid) = crate::oid::PgExtensionOid::new(d.refobjid.get())
                && let Some(ext) = self.pg_extension.get(&ext_oid)
            {
                return Some(ext.extname.as_str());
            }
        }
        None
    }

    /// Iterate all `pg_depend` rows in the catalog. Reserved for the
    /// CASCADE walker in `ddl/drop.rs`.
    pub(crate) fn iter_pg_depend(&self) -> impl Iterator<Item = &PgDepend> + '_ {
        self.pg_depend.iter()
    }

    // ── Internal-feature accessors (tests + internal feature) ───────────

    #[cfg(any(test, feature = "internal"))]
    pub fn pg_type(&self) -> &std::collections::HashMap<PgTypeOid, PgType> {
        &self.pg_type
    }

    #[cfg(any(test, feature = "internal"))]
    pub fn pg_class(&self) -> &std::collections::HashMap<PgClassOid, PgClass> {
        &self.pg_class
    }

    #[cfg(any(test, feature = "internal"))]
    pub fn pg_inherits(&self) -> &[crate::pg_catalog::PgInherits] {
        &self.pg_inherits
    }

    /// Iterate over every `pg_index` row. Tests use this to assert the
    /// shape of indexes the DDL emitted; runtime callers don't need it.
    #[cfg(any(test, feature = "internal"))]
    pub fn pg_index_values(&self) -> impl Iterator<Item = &crate::pg_catalog::PgIndex> {
        self.pg_index.values()
    }

    /// Iterate over every `pg_constraint` row. Used by the `ON CONFLICT`
    /// validator to find PK/UNIQUE constraints for a relation.
    pub(crate) fn pg_constraint_values(
        &self,
    ) -> impl Iterator<Item = &crate::pg_catalog::PgConstraint> {
        self.pg_constraint.values()
    }

    /// Names of every `pg_constraint` row attached to a relation. Returns
    /// the empty `Vec` if the schema or table is unknown.
    #[cfg(any(test, feature = "internal"))]
    pub fn pg_constraint_names_for_table(&self, schema: &str, name: &str) -> Vec<String> {
        let Some(nsoid) = self.namespace_oid(schema) else {
            return Vec::new();
        };
        let Some(class_oid) = self.class_by_qname.get(&(nsoid, name.to_owned())).copied() else {
            return Vec::new();
        };
        self.pg_constraint
            .values()
            .filter(|c| c.conrelid == class_oid)
            .map(|c| c.conname.clone())
            .collect()
    }

    #[cfg(any(test, feature = "internal"))]
    pub fn pg_proc(&self) -> &std::collections::HashMap<crate::oid::PgProcOid, PgProc> {
        &self.pg_proc
    }

    #[cfg(any(test, feature = "internal"))]
    pub fn pg_aggregate(
        &self,
    ) -> &std::collections::HashMap<crate::oid::PgProcOid, crate::pg_catalog::PgAggregate> {
        &self.pg_aggregate
    }

    #[cfg(any(test, feature = "internal"))]
    pub fn pg_operator(&self) -> &std::collections::HashMap<crate::oid::PgOperatorOid, PgOperator> {
        &self.pg_operator
    }

    #[cfg(any(test, feature = "internal"))]
    pub fn pg_cast(&self) -> &std::collections::HashMap<crate::oid::PgCastOid, PgCast> {
        &self.pg_cast
    }
}
