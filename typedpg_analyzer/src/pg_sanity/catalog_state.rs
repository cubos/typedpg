//! The catalog oracle: after a migration, the system catalogs of the live
//! server and the analyzer's must hold the same rows, but for OIDs.
//!
//! Both sides are read into the same catalog rows ([`PgCatalogSeed`]): the
//! server's through [`crate::catalog_export::export_user_objects`] (the
//! export `typedpg_seed` runs, restricted to OIDs >= `FirstNormalObjectId`),
//! the analyzer's through [`PgCatalog::to_seed`]. Each side is then
//! normalized into a [`Dump`]: one entry per row of an object the
//! migrations created, keyed by the table and the object's identity (a
//! relation by schema and name, a column by relation and name, a function
//! by schema, name and argument types…), each OID-valued field rendered as
//! the identity of the object it points at. A built-in object (OID below
//! `FirstNormalObjectId`) has the same OID on both sides, and is named
//! through the analyzer's catalog.
//!
//! Compared, from the seed rows: `pg_namespace`, `pg_class` (relkind,
//! reltype), `pg_type` (every column), `pg_enum` (order and sort values),
//! `pg_range`, `pg_attribute`, `pg_proc` (`proargdefaulttypes` too: the
//! server's from its deparsed `proargdefaults`), `pg_aggregate`,
//! `pg_operator`, `pg_cast`, `pg_extension`, `pg_language`,
//! `pg_constraint` (table constraints: keys by column name, flags),
//! `pg_index` (keys, classes, collations, flags), `pg_inherits` (index
//! inheritance included), `pg_collation`, `pg_opfamily`, `pg_opclass`, text
//! search objects, `pg_conversion`. From the side tables
//! ([`super::catalog_extras`]): `pg_class`'s persistence, row security,
//! replica identity, partition / typed-table / TOAST flags, `pg_trigger`,
//! `pg_policy`, `pg_sequence` (`seqcycle` included), constraints'
//! deferrability and foreign-key actions, domain constraints, `pg_index`'s
//! validity, immediacy, clustering, access method and column names,
//! `pg_partitioned_table`, rules, columns' storage and compression,
//! `pg_statistic_ext` (`stxkeys` by column name, `stxkind`, how many
//! expressions), `pg_publication` (FOR ALL TABLES / SEQUENCES, via root,
//! the `publish` flags, `pubgencols`), `pg_publication_rel` (whether it has
//! a row filter and a column list, `prexcept`) and
//! `pg_publication_namespace`.
//!
//! `pg_depend`: every row with an end the migrations created, both ends
//! named — `(classid, objid, objsubid)` by the object's identity, an object
//! of a catalog the seed rows don't cover (a column default, a rule, a
//! trigger, a policy, a statistics object, a publication's member, a domain
//! constraint, an operator family's member, a foreign server…) by what
//! identifies it there ([`super::catalog_extras::DepObject`]) — and the
//! deptype. The analyzer's side is its own `pg_depend` rows plus the ones
//! PG records for facts it keeps as structure, derived from that structure
//! ([`super::catalog_depend`]).
//!
//! Stored expressions ([`super::catalog_exprs`]): column defaults and
//! generation expressions, CHECK constraints, index expressions and
//! predicates, policies' USING and WITH CHECK, triggers' WHEN, partition
//! bounds and key expressions, statistics expressions, domain defaults and
//! CHECKs, routines' parameter defaults, compared as PG compares stored
//! expressions — transformed, not as text. Views' queries: PG's, resolved
//! by the analyzer as CREATE VIEW resolves one, must bind the objects the
//! analyzer's view bound, produce its columns' types, typmods and
//! collations, support the NOT NULL it infers, and make it updatable alike.
//! Rules: PG's definition, analyzed as CREATE RULE is, must make the rule
//! the analyzer keeps (event, INSTEAD, condition, RETURNING, actions and
//! their rewrites, planning errors). A server deparse is read back as the
//! migrations' session, whose deparse it is, reads it.
//!
//! Normalized, where the analyzer keeps a fact its own way: a temporary
//! schema is `pg_temp` (and its row isn't compared), another session's is
//! hidden; TOAST relations are a flag (their `pg_depend` rows aren't
//! compared); index columns are names, not `pg_attribute` rows; a view
//! column's `attnotnull` (the analyzer's inferred non-nullness) and an
//! identity column's `atthasdef` (the analyzer's "filled when omitted") are
//! not compared; `proallargtypes` / `proargmodes` / `proargnames` are
//! compared as every parameter's type, mode and name (the analyzer stores
//! them together); an index of a built-in relation (REINDEX CONCURRENTLY
//! gives it a new OID) is skipped. In `pg_depend`: the rows of a foreign
//! key's internal triggers aren't compared — they only tie the triggers to
//! their constraint (DEPENDENCY_INTERNAL; a partition's clones to their
//! parent's too), nothing can depend on a trigger it creates internally,
//! and the analyzer models the constraint, not its triggers; a row the
//! analyzer keeps for an object a DROP removed is skipped — it forgets such
//! rows lazily, the next time a DROP consults them, and none of them is
//! read before.
//!
//! Uncertain state (`ddl::uncertain`): what code the analyzer couldn't
//! follow may have changed is compared only as far as it is known — a
//! routine it may have redefined for existing (not its definition, its
//! parameter defaults or its `pg_depend` rows), a trigger it may have
//! enabled or disabled but for its state, default privileges it may have
//! altered but for their rows' dependencies; a catalog anything may have
//! changed in ends the comparisons.
//!
//! Not compared: ACLs and owners (the privilege oracle compares them), and
//! everything physical (relpages, relfilenode…), which the analyzer
//! doesn't model.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::catalog_export::FIRST_NORMAL_OBJECT_ID;
use crate::pg_catalog::{
    PgAttribute, PgCast, PgCatalog, PgCatalogSeed, PgClass, PgCollation, PgConstraint,
    PgConversion, PgEnum, PgExtension, PgLanguage, PgOpclass, PgOperator, PgOpfamily, PgProc,
    PgTsObject, PgType, RelKind,
};
use crate::qualified_name::QualifiedName;

/// One normalized row: field name → rendered value.
type Row = BTreeMap<&'static str, String>;

/// The normalized user rows of one side: `(table, identity)` → fields.
#[derive(Debug, Default)]
pub(crate) struct Dump(BTreeMap<(&'static str, String), Row>);

impl Dump {
    /// Each row as a line: table, identity and fields.
    pub(crate) fn lines(&self) -> Vec<String> {
        self.0
            .iter()
            .map(|((table, identity), row)| {
                let fields: Vec<String> = row.iter().map(|(k, v)| format!("{k}={v}")).collect();
                format!("{table} {identity} {}", fields.join(" "))
            })
            .collect()
    }
}

/// What one side's OIDs name.
struct Names<'a> {
    /// The analyzer's catalog, which names every built-in OID.
    model: &'a PgCatalog,
    ns: HashMap<u32, &'a str>,
    class: HashMap<u32, &'a PgClass>,
    typ: HashMap<u32, &'a PgType>,
    proc_: HashMap<u32, &'a PgProc>,
    opr: HashMap<u32, &'a PgOperator>,
    coll: HashMap<u32, &'a PgCollation>,
    opclass: HashMap<u32, &'a PgOpclass>,
    opfamily: HashMap<u32, &'a PgOpfamily>,
    con: HashMap<u32, &'a PgConstraint>,
    ext: HashMap<u32, &'a PgExtension>,
    lang: HashMap<u32, &'a PgLanguage>,
    cast: HashMap<u32, &'a PgCast>,
    ts: HashMap<u32, &'a PgTsObject>,
    conv: HashMap<u32, &'a PgConversion>,
    enum_: HashMap<u32, &'a PgEnum>,
    attname: HashMap<(u32, i16), &'a str>,
    /// Objects of catalogs the seed rows don't cover, `(classid, objid)` →
    /// identity: access methods, foreign-data wrappers.
    extra: &'a HashMap<(u32, u32), String>,
    /// The objects of the catalogs the seed rows don't cover that
    /// `pg_depend` rows name.
    dep_objects: &'a HashMap<(u32, u32), super::catalog_extras::DepObject>,
}

/// What a side knows beyond its seed rows.
#[derive(Default)]
pub(crate) struct Aux {
    /// The identities of objects of catalogs the seed rows don't cover:
    /// `(classid, objid)` → identity.
    pub(crate) names: HashMap<(u32, u32), String>,
    /// The rows the seed rows don't carry.
    pub(crate) extras: super::catalog_extras::Extras,
}

/// A session's temporary schema is `pg_temp_<backend number>` on the
/// server, `pg_temp_1` in the analyzer: both are `pg_temp` here (and
/// `pg_toast_temp_<n>` is `pg_toast_temp`).
fn normalize_temp_schema(name: &str) -> String {
    for prefix in ["pg_temp_", "pg_toast_temp_"] {
        if let Some(n) = name.strip_prefix(prefix)
            && !n.is_empty()
            && n.bytes().all(|b| b.is_ascii_digit())
        {
            return prefix.trim_end_matches('_').to_owned();
        }
    }
    name.to_owned()
}

fn is_user(oid: u32) -> bool {
    oid >= FIRST_NORMAL_OBJECT_ID
}

impl<'a> Names<'a> {
    fn new(seed: &'a PgCatalogSeed, model: &'a PgCatalog, aux: &'a Aux) -> Self {
        Self {
            model,
            extra: &aux.names,
            dep_objects: &aux.extras.dep_objects,
            ns: seed
                .pg_namespace
                .iter()
                .map(|n| (n.oid.get(), n.nspname.as_str()))
                .collect(),
            class: seed.pg_class.iter().map(|c| (c.oid.get(), c)).collect(),
            typ: seed.pg_type.iter().map(|t| (t.oid.get(), t)).collect(),
            proc_: seed.pg_proc.iter().map(|p| (p.oid.get(), p)).collect(),
            opr: seed.pg_operator.iter().map(|o| (o.oid.get(), o)).collect(),
            coll: seed.pg_collation.iter().map(|c| (c.oid.get(), c)).collect(),
            opclass: seed.pg_opclass.iter().map(|c| (c.oid.get(), c)).collect(),
            opfamily: seed.pg_opfamily.iter().map(|f| (f.oid.get(), f)).collect(),
            con: seed
                .pg_constraint
                .iter()
                .map(|c| (c.oid.get(), c))
                .collect(),
            ext: seed.pg_extension.iter().map(|e| (e.oid.get(), e)).collect(),
            lang: seed.pg_language.iter().map(|l| (l.oid.get(), l)).collect(),
            cast: seed.pg_cast.iter().map(|c| (c.oid.get(), c)).collect(),
            ts: seed
                .pg_ts_objects
                .iter()
                .map(|t| (t.oid.get(), t))
                .collect(),
            conv: seed
                .pg_conversion
                .iter()
                .map(|c| (c.oid.get(), c))
                .collect(),
            enum_: seed.pg_enum.iter().map(|e| (e.oid.get(), e)).collect(),
            attname: seed
                .pg_attribute
                .iter()
                .map(|a| ((a.attrelid.get(), a.attnum), a.attname.as_str()))
                .collect(),
        }
    }

    fn unknown(oid: u32) -> String {
        if is_user(oid) {
            // A user OID nothing on this side names: a dangling reference.
            format!("<dangling {oid}>")
        } else {
            format!("<builtin {oid}>")
        }
    }

    fn nsp(&self, oid: u32) -> String {
        if is_user(oid) {
            self.ns.get(&oid).map(|s| normalize_temp_schema(s))
        } else {
            self.model
                .pg_namespace
                .iter()
                .find_map(|(k, n)| (k.get() == oid).then(|| n.nspname.clone()))
        }
        .unwrap_or_else(|| Self::unknown(oid))
    }

    /// Whether `nsp` holds TOAST tables: the analyzer models a relation's
    /// TOAST table as a flag, not as a relation of its own.
    fn is_toast_schema(&self, nsp: u32) -> bool {
        matches!(self.nsp(nsp).as_str(), "pg_toast" | "pg_toast_temp")
    }

    fn is_toast_or_index(&self, relid: u32) -> bool {
        self.class.get(&relid).is_some_and(|c| {
            self.is_toast_schema(c.relnamespace.get())
                || matches!(c.relkind, RelKind::Index | RelKind::PartitionedIndex)
        })
    }

    fn qualified(&self, nsp: u32, name: &str) -> String {
        QualifiedName::new(self.nsp(nsp), name.to_owned()).to_string()
    }

    fn rel(&self, oid: u32) -> String {
        if oid == 0 {
            return "-".into();
        }
        let class = if is_user(oid) {
            self.class
                .get(&oid)
                .map(|c| (c.relnamespace.get(), c.relname.clone()))
        } else {
            self.model
                .pg_class
                .get(&crate::oid::PgClassOid::from_raw(oid))
                .map(|c| (c.relnamespace.get(), c.relname.clone()))
        };
        match class {
            Some((nsp, name)) => self.qualified(nsp, &name),
            None => Self::unknown(oid),
        }
    }

    fn att(&self, relid: u32, attnum: i16) -> String {
        if attnum == 0 {
            return "expr".into();
        }
        let name = if is_user(relid) {
            self.attname.get(&(relid, attnum)).map(|s| (*s).to_owned())
        } else {
            self.model
                .attributes_of(crate::oid::PgClassOid::from_raw(relid))
                .iter()
                .find(|a| a.attnum == attnum)
                .map(|a| a.attname.clone())
        };
        name.unwrap_or_else(|| format!("#{attnum}"))
    }

    fn atts(&self, relid: u32, attnums: &[i16]) -> String {
        attnums
            .iter()
            .map(|&n| self.att(relid, n))
            .collect::<Vec<_>>()
            .join(",")
    }

    fn ty(&self, oid: u32) -> String {
        if oid == 0 {
            return "-".into();
        }
        let t = if is_user(oid) {
            self.typ
                .get(&oid)
                .map(|t| (t.typnamespace.get(), t.typname.clone()))
        } else {
            self.model
                .pg_type
                .get(&crate::oid::PgTypeOid::from_raw(oid))
                .map(|t| (t.typnamespace.get(), t.typname.clone()))
        };
        match t {
            Some((nsp, name)) => self.qualified(nsp, &name),
            None => Self::unknown(oid),
        }
    }

    fn types(&self, oids: impl IntoIterator<Item = u32>) -> String {
        oids.into_iter()
            .map(|t| self.ty(t))
            .collect::<Vec<_>>()
            .join(",")
    }

    fn proc_(&self, oid: u32) -> String {
        if oid == 0 {
            return "-".into();
        }
        let p = if is_user(oid) {
            self.proc_.get(&oid).map(|p| {
                (
                    p.pronamespace.get(),
                    p.proname.clone(),
                    p.proargtypes.iter().map(|t| t.get()).collect::<Vec<_>>(),
                )
            })
        } else {
            self.model
                .pg_proc
                .get(&crate::oid::PgProcOid::from_raw(oid))
                .map(|p| {
                    (
                        p.pronamespace.get(),
                        p.proname.clone(),
                        p.proargtypes.iter().map(|t| t.get()).collect(),
                    )
                })
        };
        match p {
            Some((nsp, name, args)) => {
                format!("{}({})", self.qualified(nsp, &name), self.types(args))
            }
            None => Self::unknown(oid),
        }
    }

    fn opr(&self, oid: u32) -> String {
        if oid == 0 {
            return "-".into();
        }
        let o = if is_user(oid) {
            self.opr.get(&oid).map(|o| {
                (
                    o.oprnamespace.get(),
                    o.oprname.clone(),
                    o.oprleft.map_or(0, |t| t.get()),
                    o.oprright.get(),
                )
            })
        } else {
            self.model
                .pg_operator
                .get(&crate::oid::PgOperatorOid::from_raw(oid))
                .map(|o| {
                    (
                        o.oprnamespace.get(),
                        o.oprname.clone(),
                        o.oprleft.map_or(0, |t| t.get()),
                        o.oprright.get(),
                    )
                })
        };
        match o {
            Some((nsp, name, l, r)) => {
                format!(
                    "{}({},{})",
                    self.qualified(nsp, &name),
                    self.ty(l),
                    self.ty(r)
                )
            }
            None => Self::unknown(oid),
        }
    }

    fn coll(&self, oid: u32) -> String {
        if oid == 0 {
            return "-".into();
        }
        let c = if is_user(oid) {
            self.coll
                .get(&oid)
                .map(|c| (c.collnamespace.get(), c.collname.clone()))
        } else {
            self.model
                .pg_collation
                .get(&crate::oid::PgCollationOid::from_raw(oid))
                .map(|c| (c.collnamespace.get(), c.collname.clone()))
        };
        match c {
            Some((nsp, name)) => self.qualified(nsp, &name),
            None => Self::unknown(oid),
        }
    }

    fn opclass(&self, oid: u32) -> String {
        if oid == 0 {
            return "-".into();
        }
        let c = if is_user(oid) {
            self.opclass
                .get(&oid)
                .map(|c| (c.opcmethod.clone(), c.opcnamespace.get(), c.opcname.clone()))
        } else {
            self.model
                .pg_opclass
                .iter()
                .find(|c| c.oid.get() == oid)
                .map(|c| (c.opcmethod.clone(), c.opcnamespace.get(), c.opcname.clone()))
        };
        match c {
            Some((am, nsp, name)) => format!("{} USING {am}", self.qualified(nsp, &name)),
            None => Self::unknown(oid),
        }
    }

    fn opfamily(&self, oid: u32) -> String {
        let f = if is_user(oid) {
            self.opfamily
                .get(&oid)
                .map(|f| (f.opfmethod.clone(), f.opfnamespace.get(), f.opfname.clone()))
        } else {
            self.model
                .pg_opfamily
                .iter()
                .find(|f| f.oid.get() == oid)
                .map(|f| (f.opfmethod.clone(), f.opfnamespace.get(), f.opfname.clone()))
        };
        match f {
            Some((am, nsp, name)) => format!("{} USING {am}", self.qualified(nsp, &name)),
            None => Self::unknown(oid),
        }
    }

    fn lang(&self, oid: u32) -> String {
        // initdb's languages (plpgsql) are unpinned: their OIDs may differ
        // from the seed's on another build of the server; the side names
        // them itself when it can.
        if let Some(name) = self.extra.get(&(2612, oid)) {
            return name.clone();
        }
        if is_user(oid) {
            self.lang.get(&oid).map(|l| l.lanname.clone())
        } else {
            self.model
                .pg_language
                .get(&crate::oid::PgLanguageOid::from_raw(oid))
                .map(|l| l.lanname.clone())
        }
        .unwrap_or_else(|| Self::unknown(oid))
    }

    fn ext(&self, oid: u32) -> String {
        if !is_user(oid) {
            // initdb's (plpgsql), named through the analyzer's catalog.
            return self
                .model
                .pg_extension
                .get(&crate::oid::PgExtensionOid::from_raw(oid))
                .map_or_else(|| Self::unknown(oid), |e| e.extname.clone());
        }
        self.ext
            .get(&oid)
            .map(|e| e.extname.clone())
            .unwrap_or_else(|| Self::unknown(oid))
    }

    fn constraint(&self, oid: u32) -> String {
        match self.con.get(&oid) {
            Some(c) => format!("{} on {}", c.conname, self.rel(c.conrelid.get())),
            None => Self::unknown(oid),
        }
    }

    fn cast(&self, c: &PgCast) -> String {
        format!(
            "({} AS {})",
            self.ty(c.castsource.get()),
            self.ty(c.casttarget.get())
        )
    }

    fn ts(&self, t: &PgTsObject) -> String {
        format!("{} {}", t.kind, self.qualified(t.namespace.get(), &t.name))
    }

    /// The identity of the object `pg_depend` names by `(classid, objid,
    /// objsubid)`.
    fn object(&self, classid: u32, objid: u32, objsubid: i16) -> String {
        use crate::ddl::depend as d;
        use crate::pg_catalog as c;
        let classid_oid = crate::oid::PgClassOid::from_raw(classid);
        if let Some(object) = self.dep_objects.get(&(classid, objid)) {
            self.dep_object(object)
        } else if classid_oid == c::PG_CLASS_RELID {
            if objsubid != 0 {
                format!("column {}.{}", self.rel(objid), self.att(objid, objsubid))
            } else {
                format!("relation {}", self.rel(objid))
            }
        } else if classid_oid == c::PG_TYPE_RELID {
            format!("type {}", self.ty(objid))
        } else if classid_oid == c::PG_PROC_RELID {
            format!("function {}", self.proc_(objid))
        } else if classid_oid == c::PG_NAMESPACE_RELID {
            format!("schema {}", self.nsp(objid))
        } else if classid_oid == c::PG_OPERATOR_RELID {
            format!("operator {}", self.opr(objid))
        } else if classid_oid == c::PG_COLLATION_RELID {
            format!("collation {}", self.coll(objid))
        } else if classid_oid == c::PG_EXTENSION_RELID {
            format!("extension {}", self.ext(objid))
        } else if classid_oid == c::PG_LANGUAGE_RELID {
            format!("language {}", self.lang(objid))
        } else if classid_oid == c::PG_CAST_RELID {
            match self.cast.get(&objid) {
                Some(cast) => format!("cast {}", self.cast(cast)),
                None => format!("cast {}", Self::unknown(objid)),
            }
        } else if classid_oid == d::PG_CONSTRAINT_RELID {
            format!("constraint {}", self.constraint(objid))
        } else if classid_oid == d::PG_OPCLASS_RELID {
            format!("operator class {}", self.opclass(objid))
        } else if classid_oid == d::PG_OPFAMILY_RELID {
            format!("operator family {}", self.opfamily(objid))
        } else if [
            d::PG_TS_CONFIG_RELID,
            d::PG_TS_DICT_RELID,
            d::PG_TS_PARSER_RELID,
            d::PG_TS_TEMPLATE_RELID,
        ]
        .contains(&classid_oid)
        {
            let builtin = || {
                self.model
                    .pg_ts_objects
                    .iter()
                    .find(|t| !is_user(objid) && t.oid.get() == objid)
            };
            match self.ts.get(&objid).copied().or_else(builtin) {
                Some(t) => format!("text search {}", self.ts(t)),
                None => format!("text search object {}", Self::unknown(objid)),
            }
        } else if classid == 2607 {
            let builtin = || {
                self.model
                    .conversions
                    .iter()
                    .find(|c| !is_user(objid) && c.oid.get() == objid)
            };
            match self.conv.get(&objid).copied().or_else(builtin) {
                Some(conv) => format!(
                    "conversion {}",
                    self.qualified(conv.namespace.get(), &conv.name)
                ),
                None => format!("conversion {}", Self::unknown(objid)),
            }
        } else if classid == 3501 {
            match self.enum_.get(&objid) {
                Some(e) => format!("enum label {}.{}", self.ty(e.enumtypid.get()), e.enumlabel),
                None => format!("enum label {}", Self::unknown(objid)),
            }
        } else if let Some(name) = self.extra.get(&(classid, objid)) {
            name.clone()
        } else {
            // A catalog the seed rows don't cover (pg_attrdef, pg_trigger,
            // pg_rewrite, pg_policy…): its OIDs can't be normalized.
            format!("<catalog {classid}> {}", Self::unknown(objid))
        }
    }
}

impl Names<'_> {
    fn dep_object(&self, object: &super::catalog_extras::DepObject) -> String {
        use super::catalog_extras::DepObject as O;
        match object {
            O::Attrdef { relid, attnum } => {
                format!(
                    "default of {}.{}",
                    self.rel(*relid),
                    self.att(*relid, *attnum)
                )
            }
            O::Rule { relid, name } => format!("rule {name} on {}", self.rel(*relid)),
            O::Trigger { relid, name } => format!("trigger {name} on {}", self.rel(*relid)),
            O::InternalTrigger {
                relid,
                constraint,
                function,
            } => format!(
                "internal trigger {} on {} of {}",
                self.proc_(*function),
                self.rel(*relid),
                self.constraint(*constraint)
            ),
            O::Policy { relid, name } => format!("policy {name} on {}", self.rel(*relid)),
            O::Statistics { nsp, name } => format!("statistics {}", self.qualified(*nsp, name)),
            O::Publication(name) => format!("publication {name}"),
            O::PublicationRel { publication, relid } => {
                format!("publication {publication} of {}", self.rel(*relid))
            }
            O::PublicationNamespace { publication, nsp } => {
                format!("publication {publication} of schema {}", self.nsp(*nsp))
            }
            O::DomainConstraint { typid, name } => {
                format!("constraint {name} on domain {}", self.ty(*typid))
            }
            O::EventTrigger(name) => format!("event trigger {name}"),
            O::Server(name) => format!("server {name}"),
            O::Subscription(name) => format!("subscription {name}"),
            O::UserMapping { user, server } => format!("user mapping for {user} on {server}"),
            O::FamilyMember {
                func,
                family,
                number,
                left,
                right,
            } => format!(
                "{} {number} ({}, {}) of {}",
                if *func { "function" } else { "operator" },
                self.ty(*left),
                self.ty(*right),
                self.opfamily(*family)
            ),
            O::Transform { typ, lang } => {
                format!("transform for {} language {lang}", self.ty(*typ))
            }
            O::Conversion { nsp, name } => format!("conversion {}", self.qualified(*nsp, name)),
            O::DefaultAcl { role, nsp, objtype } => format!(
                "default privileges of {role} on {objtype} in {}",
                if *nsp == 0 {
                    "-".into()
                } else {
                    self.nsp(*nsp)
                }
            ),
        }
    }

    /// Whether the object `(classid, objid)` is a TOAST relation (or one
    /// of its columns): the analyzer models a TOAST table as a flag.
    fn is_toast_object(&self, classid: u32, objid: u32) -> bool {
        classid == crate::pg_catalog::PG_CLASS_RELID.get()
            && self
                .class
                .get(&objid)
                .is_some_and(|c| self.is_toast_schema(c.relnamespace.get()))
    }
}

fn b(v: bool) -> String {
    v.to_string()
}

fn opt<T: std::fmt::Debug>(v: Option<T>) -> String {
    v.map_or_else(|| "-".into(), |v| format!("{v:?}"))
}

/// Drop the rows of the objects in `hidden` namespaces (another session's
/// temporary schema: the query session's helper functions).
pub(crate) fn hide_namespaces(seed: &mut PgCatalogSeed, hidden: &HashSet<u32>) {
    if hidden.is_empty() {
        return;
    }
    let ns = |oid: crate::oid::PgNamespaceOid| hidden.contains(&oid.get());
    seed.pg_namespace.retain(|r| !hidden.contains(&r.oid.get()));
    let classes: HashSet<u32> = seed
        .pg_class
        .iter()
        .filter(|c| ns(c.relnamespace))
        .map(|c| c.oid.get())
        .collect();
    let types: HashSet<u32> = seed
        .pg_type
        .iter()
        .filter(|t| ns(t.typnamespace))
        .map(|t| t.oid.get())
        .collect();
    let procs: HashSet<u32> = seed
        .pg_proc
        .iter()
        .filter(|p| ns(p.pronamespace))
        .map(|p| p.oid.get())
        .collect();
    let constraints: HashSet<u32> = seed
        .pg_constraint
        .iter()
        .filter(|c| classes.contains(&c.conrelid.get()))
        .map(|c| c.oid.get())
        .collect();
    seed.pg_class.retain(|r| !classes.contains(&r.oid.get()));
    seed.pg_type.retain(|r| !types.contains(&r.oid.get()));
    seed.pg_proc.retain(|r| !procs.contains(&r.oid.get()));
    seed.pg_attribute
        .retain(|r| !classes.contains(&r.attrelid.get()));
    seed.pg_constraint
        .retain(|r| !constraints.contains(&r.oid.get()));
    seed.pg_index
        .retain(|r| !classes.contains(&r.indrelid.get()));
    seed.pg_inherits
        .retain(|r| !classes.contains(&r.inhrelid.get()));
    seed.pg_operator.retain(|r| !ns(r.oprnamespace));
    seed.pg_collation.retain(|r| !ns(r.collnamespace));
    let hidden_objects: HashSet<u32> = classes
        .iter()
        .chain(&types)
        .chain(&procs)
        .chain(&constraints)
        .copied()
        .collect();
    seed.pg_depend.retain(|d| {
        !hidden_objects.contains(&d.objid.get()) && !hidden_objects.contains(&d.refobjid.get())
    });
}

/// Normalize the user rows of `seed` (rows of built-in objects are
/// ignored), naming built-ins through `model`.
pub(crate) fn normalize(seed: &PgCatalogSeed, model: &PgCatalog, aux: &Aux) -> Dump {
    let n = Names::new(seed, model, aux);
    let mut dump = Dump::default();
    let mut put = |table: &'static str, key: String, row: Row| {
        // Two rows with one identity: keep both, distinguished.
        let mut key = key;
        while dump.0.contains_key(&(table, key.clone())) {
            key.push_str(" (dup)");
        }
        dump.0.insert((table, key), row);
    };

    // A temporary schema's row is not compared (PG keeps it once created,
    // the analyzer while the session has a temporary object): its objects
    // are.
    for ns in seed.pg_namespace.iter().filter(|r| {
        is_user(r.oid.get()) && !n.is_toast_schema(r.oid.get()) && n.nsp(r.oid.get()) != "pg_temp"
    }) {
        put("pg_namespace", n.nsp(ns.oid.get()), Row::new());
    }
    // An index of a built-in relation: REINDEX CONCURRENTLY gave it a new
    // OID (the analyzer keeps the old one, `rebuilt_index_oids`).
    let builtin_index: HashSet<u32> = seed
        .pg_index
        .iter()
        .filter(|i| !is_user(i.indrelid.get()))
        .map(|i| i.indexrelid.get())
        .collect();
    for c in seed.pg_class.iter().filter(|r| {
        is_user(r.oid.get())
            && !n.is_toast_schema(r.relnamespace.get())
            && !builtin_index.contains(&r.oid.get())
    }) {
        put(
            "pg_class",
            n.rel(c.oid.get()),
            Row::from([
                ("relkind", format!("{:?}", c.relkind)),
                ("reltype", n.ty(c.reltype.map_or(0, |t| t.get()))),
            ]),
        );
    }
    for t in seed.pg_type.iter().filter(|r| is_user(r.oid.get())) {
        let oid = |o: Option<u32>| o.unwrap_or(0);
        put(
            "pg_type",
            n.ty(t.oid.get()),
            Row::from([
                ("typtype", format!("{:?}", t.typtype)),
                ("typcategory", format!("{:?}", t.typcategory)),
                ("typispreferred", b(t.typispreferred)),
                ("typrelid", n.rel(oid(t.typrelid.map(|x| x.get())))),
                ("typelem", n.ty(oid(t.typelem.map(|x| x.get())))),
                ("typarray", n.ty(oid(t.typarray.map(|x| x.get())))),
                ("typbasetype", n.ty(oid(t.typbasetype.map(|x| x.get())))),
                ("typnotnull", b(t.typnotnull)),
                ("typtypmod", opt(t.typtypmod)),
                ("typcollation", n.coll(oid(t.typcollation.map(|x| x.get())))),
                ("typstorage", format!("{:?}", t.typstorage)),
                ("typlen", t.typlen.to_string()),
                ("typbyval", b(t.typbyval)),
                ("typalign", format!("{:?}", t.typalign)),
                (
                    "typsubscript",
                    n.proc_(oid(t.typsubscript.map(|x| x.get()))),
                ),
                ("typisdefined", b(t.typisdefined)),
                ("typdelim", t.typdelim.to_string()),
            ]),
        );
    }
    {
        // Labels in sort order: the order is what PG compares by; the
        // float values themselves are compared too.
        let mut by_type: BTreeMap<u32, Vec<&PgEnum>> = BTreeMap::new();
        for e in seed.pg_enum.iter().filter(|r| is_user(r.enumtypid.get())) {
            by_type.entry(e.enumtypid.get()).or_default().push(e);
        }
        for (typ, mut labels) in by_type {
            labels.sort_by(|a, b| a.enumsortorder.total_cmp(&b.enumsortorder));
            for (rank, e) in labels.iter().enumerate() {
                put(
                    "pg_enum",
                    format!("{}.{}", n.ty(typ), e.enumlabel),
                    Row::from([
                        ("rank", rank.to_string()),
                        ("enumsortorder", e.enumsortorder.to_string()),
                    ]),
                );
            }
        }
    }
    for r in seed.pg_range.iter().filter(|r| is_user(r.rngtypid.get())) {
        put(
            "pg_range",
            n.ty(r.rngtypid.get()),
            Row::from([
                ("rngsubtype", n.ty(r.rngsubtype.get())),
                (
                    "rngmultitypid",
                    n.ty(r.rngmultitypid.map_or(0, |t| t.get())),
                ),
                (
                    "rngcollation",
                    n.coll(r.rngcollation.map_or(0, |t| t.get())),
                ),
                ("rngsubopc", n.opclass(r.rngsubopc.map_or(0, |t| t.get()))),
            ]),
        );
    }
    // Index columns: the analyzer keeps their names aside
    // (`index_attnames`), not as pg_attribute rows.
    for a in seed
        .pg_attribute
        .iter()
        .filter(|r| is_user(r.attrelid.get()) && !n.is_toast_or_index(r.attrelid.get()))
    {
        put("pg_attribute", attribute_key(&n, a), attribute_row(&n, a));
    }
    for p in seed.pg_proc.iter().filter(|r| is_user(r.oid.get())) {
        put(
            "pg_proc",
            n.proc_(p.oid.get()),
            Row::from([
                ("prokind", format!("{:?}", p.prokind)),
                ("prorettype", n.ty(p.prorettype.get())),
                ("proretset", b(p.proretset)),
                ("provariadic", n.ty(p.provariadic.map_or(0, |t| t.get()))),
                ("proisstrict", b(p.proisstrict)),
                ("pronargdefaults", p.pronargdefaults.to_string()),
                (
                    "proargdefaulttypes",
                    n.types(p.proargdefaulttypes.iter().map(|t| t.get())),
                ),
                // PG stores proallargtypes / proargmodes only when some
                // parameter is not IN, and proargnames when some parameter
                // is named; the analyzer keeps the three together when
                // either holds. Compared: every parameter's type, mode and
                // name.
                (
                    "proallargtypes",
                    n.types(
                        if p.proallargtypes.is_empty() {
                            &p.proargtypes
                        } else {
                            &p.proallargtypes
                        }
                        .iter()
                        .map(|t| t.get()),
                    ),
                ),
                (
                    "proargmodes",
                    if p.proargmodes.is_empty() {
                        format!(
                            "{:?}",
                            vec![crate::pg_catalog::ArgMode::In; p.proargtypes.len()]
                        )
                    } else {
                        format!("{:?}", p.proargmodes)
                    },
                ),
                (
                    "proargnames",
                    if p.proargnames.iter().all(String::is_empty) {
                        "[]".to_owned()
                    } else {
                        format!("{:?}", p.proargnames)
                    },
                ),
                ("provolatile", format!("{:?}", p.provolatile)),
                ("prolang", n.lang(p.prolang.get())),
                ("prosupport", n.proc_(p.prosupport.map_or(0, |t| t.get()))),
            ]),
        );
    }
    for a in seed
        .pg_aggregate
        .iter()
        .filter(|r| is_user(r.aggfnoid.get()))
    {
        put(
            "pg_aggregate",
            n.proc_(a.aggfnoid.get()),
            Row::from([
                ("aggfinalfn", n.proc_(a.aggfinalfn.map_or(0, |t| t.get()))),
                ("aggkind", format!("{:?}", a.aggkind)),
                ("aggnumdirectargs", a.aggnumdirectargs.to_string()),
            ]),
        );
    }
    for o in seed.pg_operator.iter().filter(|r| is_user(r.oid.get())) {
        put(
            "pg_operator",
            n.opr(o.oid.get()),
            Row::from([
                ("oprresult", n.ty(o.oprresult.map_or(0, |t| t.get()))),
                ("oprcode", n.proc_(o.oprcode.map_or(0, |t| t.get()))),
                ("oprcom", n.opr(o.oprcom.map_or(0, |t| t.get()))),
                ("oprnegate", n.opr(o.oprnegate.map_or(0, |t| t.get()))),
            ]),
        );
    }
    for c in seed.pg_cast.iter().filter(|r| is_user(r.oid.get())) {
        put(
            "pg_cast",
            n.cast(c),
            Row::from([
                ("castcontext", format!("{:?}", c.castcontext)),
                ("castmethod", format!("{:?}", c.castmethod)),
                ("castfunc", n.proc_(c.castfunc.map_or(0, |t| t.get()))),
            ]),
        );
    }
    for e in seed.pg_extension.iter().filter(|r| is_user(r.oid.get())) {
        put(
            "pg_extension",
            e.extname.clone(),
            Row::from([
                ("extnamespace", n.nsp(e.extnamespace.get())),
                ("extversion", e.extversion.clone()),
            ]),
        );
    }
    for l in seed.pg_language.iter().filter(|r| is_user(r.oid.get())) {
        put(
            "pg_language",
            l.lanname.clone(),
            Row::from([
                ("lanispl", b(l.lanispl)),
                ("lanpltrusted", b(l.lanpltrusted)),
                (
                    "lanplcallfoid",
                    n.proc_(l.lanplcallfoid.map_or(0, |t| t.get())),
                ),
            ]),
        );
    }
    for c in seed.pg_constraint.iter().filter(|r| is_user(r.oid.get())) {
        let rel = c.conrelid.get();
        let frel = c.confrelid.map_or(0, |t| t.get());
        put(
            "pg_constraint",
            n.constraint(c.oid.get()),
            Row::from([
                ("contype", format!("{:?}", c.contype)),
                ("conkey", n.atts(rel, &c.conkey)),
                ("confrelid", n.rel(frel)),
                ("confkey", n.atts(frel, &c.confkey)),
                ("conislocal", b(c.conislocal)),
                ("coninhcount", c.coninhcount.to_string()),
                ("conenforced", b(c.conenforced)),
                ("convalidated", b(c.convalidated)),
                ("connoinherit", b(c.connoinherit)),
                ("conperiod", b(c.conperiod)),
            ]),
        );
    }
    for i in seed.pg_index.iter().filter(|r| {
        is_user(r.indexrelid.get())
            && is_user(r.indrelid.get())
            && !n.is_toast_or_index(r.indrelid.get())
    }) {
        let rel = i.indrelid.get();
        put(
            "pg_index",
            n.rel(i.indexrelid.get()),
            Row::from([
                ("indrelid", n.rel(rel)),
                ("indnatts", i.indnatts.to_string()),
                ("indnkeyatts", i.indnkeyatts.to_string()),
                ("indisunique", b(i.indisunique)),
                ("indisprimary", b(i.indisprimary)),
                ("indisexclusion", b(i.indisexclusion)),
                ("indkey", n.atts(rel, &i.indkey)),
                ("indexprs", i.indexprs.len().to_string()),
                ("indpred", b(i.indpred.is_some())),
                (
                    "indcollation",
                    i.indcollation
                        .iter()
                        .map(|c| n.coll(c.map_or(0, |c| c.get())))
                        .collect::<Vec<_>>()
                        .join(","),
                ),
                (
                    "indclass",
                    i.indclass
                        .iter()
                        .map(|c| n.opclass(c.map_or(0, |c| c.get())))
                        .collect::<Vec<_>>()
                        .join(","),
                ),
            ]),
        );
    }
    for i in seed
        .pg_inherits
        .iter()
        .filter(|r| is_user(r.inhrelid.get()))
    {
        put(
            "pg_inherits",
            format!("{} #{}", n.rel(i.inhrelid.get()), i.inhseqno),
            Row::from([("inhparent", n.rel(i.inhparent.get()))]),
        );
    }
    for c in seed.pg_collation.iter().filter(|r| is_user(r.oid.get())) {
        put(
            "pg_collation",
            n.coll(c.oid.get()),
            Row::from([
                ("collencoding", c.collencoding.to_string()),
                ("collisdeterministic", b(c.collisdeterministic)),
            ]),
        );
    }
    for f in seed.pg_opfamily.iter().filter(|r| is_user(r.oid.get())) {
        put("pg_opfamily", n.opfamily(f.oid.get()), Row::new());
    }
    for c in seed.pg_opclass.iter().filter(|r| is_user(r.oid.get())) {
        put(
            "pg_opclass",
            n.opclass(c.oid.get()),
            Row::from([
                ("opcintype", n.ty(c.opcintype.get())),
                ("opcdefault", b(c.opcdefault)),
                (
                    "opcfamily",
                    format!(
                        "{} USING {}",
                        n.qualified(c.opcfamilynamespace.get(), &c.opcfamily),
                        c.opcmethod
                    ),
                ),
            ]),
        );
    }
    for t in seed.pg_ts_objects.iter().filter(|r| is_user(r.oid.get())) {
        put("pg_ts_*", n.ts(t), Row::new());
    }
    for c in seed.pg_conversion.iter().filter(|r| is_user(r.oid.get())) {
        put(
            "pg_conversion",
            n.qualified(c.namespace.get(), &c.name),
            Row::new(),
        );
    }
    {
        // pg_depend: a set of edges, each `object -> referenced (deptype)`.
        let edges: BTreeSet<String> = seed
            .pg_depend
            .iter()
            .filter(|d| is_user(d.objid.get()) || is_user(d.refobjid.get()))
            .filter(|d| {
                // A foreign key's triggers are internal to it: their rows
                // only tie them to it (DEPENDENCY_INTERNAL, partition clones
                // to their parent's), and nothing can depend on them.
                !matches!(
                    n.dep_objects.get(&(d.classid.get(), d.objid.get())),
                    Some(super::catalog_extras::DepObject::InternalTrigger { .. })
                ) && !n.is_toast_object(d.classid.get(), d.objid.get())
                    && !n.is_toast_object(d.refclassid.get(), d.refobjid.get())
                    && !builtin_index.contains(&d.objid.get())
            })
            .map(|d| {
                let kind = aux.extras.partition_deps.get(&(
                    d.classid.get(),
                    d.objid.get(),
                    d.objsubid,
                    d.refclassid.get(),
                    d.refobjid.get(),
                    d.refobjsubid,
                ));
                format!(
                    "{} -> {} ({})",
                    n.object(d.classid.get(), d.objid.get(), d.objsubid),
                    n.object(d.refclassid.get(), d.refobjid.get(), d.refobjsubid),
                    match kind {
                        Some(true) => "PartitionPri".to_owned(),
                        Some(false) => "PartitionSec".to_owned(),
                        None => format!("{:?}", d.deptype),
                    }
                )
            })
            .collect();
        for edge in edges {
            put("pg_depend", edge, Row::new());
        }
    }
    put_extras(&n, &aux.extras, &mut put);
    dump
}

/// The rows of `x` (see [`super::catalog_extras`]), of the relations and
/// types `n`'s side has (another session's temporary objects are not).
fn put_extras(
    n: &Names<'_>,
    x: &super::catalog_extras::Extras,
    put: &mut dyn FnMut(&'static str, String, Row),
) {
    let has_rel = |relid: u32| {
        is_user(relid)
            && n.class
                .get(&relid)
                .is_some_and(|c| !n.is_toast_schema(c.relnamespace.get()))
    };
    let names = |v: &[String]| v.join(",");
    for c in x.classes.iter().filter(|c| has_rel(c.relid)) {
        put(
            "pg_class.flags",
            n.rel(c.relid),
            Row::from([
                ("relpersistence", c.persistence.to_string()),
                ("relrowsecurity", b(c.rowsecurity)),
                ("relforcerowsecurity", b(c.forcerowsecurity)),
                ("relreplident", c.replident.to_string()),
                ("relispartition", b(c.ispartition)),
                ("reloftype", n.ty(c.oftype)),
                ("has_toast", b(c.hastoast)),
            ]),
        );
    }
    for t in x.triggers.iter().filter(|t| has_rel(t.relid)) {
        put(
            "pg_trigger",
            format!("{} on {}", t.name, n.rel(t.relid)),
            Row::from([
                ("tgfoid", n.proc_(t.function)),
                ("tgtype", t.tgtype.to_string()),
                ("tgenabled", t.enabled.to_string()),
                ("tgattr", names(&t.columns)),
                ("tgconstrrelid", n.rel(t.constrrel)),
                ("tgparentid_rel", n.rel(t.parent_rel)),
                ("tgconstraint", b(t.constraint)),
                ("tgdeferrable", b(t.deferrable)),
                ("tginitdeferred", b(t.initdeferred)),
                ("tgqual", b(t.has_when)),
                ("tgoldtable", t.old_table.clone()),
                ("tgnewtable", t.new_table.clone()),
            ]),
        );
    }
    for p in x.policies.iter().filter(|p| has_rel(p.relid)) {
        put(
            "pg_policy",
            format!("{} on {}", p.name, n.rel(p.relid)),
            Row::from([
                ("polcmd", p.cmd.to_string()),
                ("polpermissive", b(p.permissive)),
                ("polroles", names(&p.roles)),
            ]),
        );
    }
    for s in x.sequences.iter().filter(|s| has_rel(s.relid)) {
        put(
            "pg_sequence",
            n.rel(s.relid),
            Row::from([
                ("seqtypid", n.ty(s.typ)),
                ("seqstart", s.start.to_string()),
                ("seqincrement", s.increment.to_string()),
                ("seqmax", s.max.to_string()),
                ("seqmin", s.min.to_string()),
                ("seqcache", s.cache.to_string()),
                ("seqcycle", b(s.cycle)),
            ]),
        );
    }
    for p in &x.publications {
        put(
            "pg_publication",
            p.name.clone(),
            Row::from([
                ("puballtables", b(p.alltables)),
                ("puballsequences", b(p.allsequences)),
                ("pubviaroot", b(p.viaroot)),
                ("publish", format!("{:?}", p.publish)),
                ("pubgencols", p.gencols.to_string()),
            ]),
        );
        for &(relid, qual, cols, except) in p.relations.iter().filter(|r| has_rel(r.0)) {
            put(
                "pg_publication_rel",
                format!("{} of {}", p.name, n.rel(relid)),
                Row::from([
                    ("prqual", b(qual)),
                    ("prattrs", b(cols)),
                    ("prexcept", b(except)),
                ]),
            );
        }
        for &nsp in &p.schemas {
            put(
                "pg_publication_namespace",
                format!("{} of {}", p.name, n.nsp(nsp)),
                Row::new(),
            );
        }
    }
    for s in x.statistics.iter().filter(|s| has_rel(s.relid)) {
        put(
            "pg_statistic_ext",
            n.qualified(s.nsp, &s.name),
            Row::from([
                ("stxrelid", n.rel(s.relid)),
                ("stxkeys", n.atts(s.relid, &s.keys)),
                ("stxkind", s.kinds.iter().collect::<String>()),
                ("stxexprs", s.exprs.to_string()),
            ]),
        );
    }
    for c in &x.constraints {
        let Some(con) = n.con.get(&c.oid) else {
            continue;
        };
        if !has_rel(con.conrelid.get()) {
            continue;
        }
        let mut row = Row::from([("condeferrable", b(c.deferrable))]);
        if let Some(d) = c.initdeferred {
            row.insert("condeferred", b(d));
        }
        if let Some((upd, del, matchtype)) = c.fk {
            row.insert("confupdtype", upd.to_string());
            row.insert("confdeltype", del.to_string());
            row.insert("confmatchtype", matchtype.to_string());
        }
        put("pg_constraint.extra", n.constraint(c.oid), row);
    }
    for c in x
        .domain_constraints
        .iter()
        .filter(|c| n.typ.contains_key(&c.typid))
    {
        put(
            "pg_constraint.domain",
            format!("{} on {}", c.name, n.ty(c.typid)),
            Row::from([("contype", c.contype.to_string())]),
        );
    }
    for i in &x.indexes {
        let Some(rel) = n.class.get(&i.indexrelid).map(|c| c.relnamespace.get()) else {
            continue;
        };
        if n.is_toast_schema(rel) {
            continue;
        }
        put(
            "pg_index.extra",
            n.rel(i.indexrelid),
            Row::from([
                ("amname", i.am.clone()),
                ("indisvalid", b(i.valid)),
                ("indimmediate", b(i.immediate)),
                ("indisclustered", b(i.clustered)),
                ("indisreplident", b(i.replident)),
                ("indnullsnotdistinct", b(i.nulls_not_distinct)),
                ("columns", names(&i.columns)),
            ]),
        );
    }
    for p in x.partitioned.iter().filter(|p| has_rel(p.relid)) {
        put(
            "pg_partitioned_table",
            n.rel(p.relid),
            Row::from([
                ("partstrat", p.strategy.to_string()),
                ("partattrs", n.atts(p.relid, &p.attnums)),
            ]),
        );
    }
    for r in x.rules.iter().filter(|r| has_rel(r.relid)) {
        put(
            "pg_rewrite",
            format!("{} on {}", r.name, n.rel(r.relid)),
            Row::from([
                ("ev_type", r.event.to_string()),
                ("is_instead", b(r.instead)),
                ("fires", b(r.fires)),
            ]),
        );
    }
    for a in x.attributes.iter().filter(|a| has_rel(a.relid)) {
        put(
            "pg_attribute.storage",
            format!("{}.{}", n.rel(a.relid), n.att(a.relid, a.attnum)),
            Row::from([
                ("attstorage", a.storage.to_string()),
                ("attcompression", format!("{:?}", a.compression)),
            ]),
        );
    }
}

fn attribute_key(n: &Names<'_>, a: &PgAttribute) -> String {
    format!("{}.{}", n.rel(a.attrelid.get()), a.attname)
}

fn attribute_row(n: &Names<'_>, a: &PgAttribute) -> Row {
    // A view's columns: the analyzer keeps the non-nullness it inferred in
    // attnotnull (PG leaves it false).
    let view = n
        .class
        .get(&a.attrelid.get())
        .is_some_and(|c| matches!(c.relkind, RelKind::View | RelKind::MaterializedView));
    Row::from([
        ("atttypid", n.ty(a.atttypid.get())),
        ("attnum", a.attnum.to_string()),
        (
            "attnotnull",
            if view { "-".into() } else { b(a.attnotnull) },
        ),
        // An identity column: the analyzer's atthasdef says the column gets
        // a value when an INSERT omits it (PG's is false: no pg_attrdef).
        ("atthasdef", b(a.atthasdef && a.attidentity.is_none())),
        ("attgenerated", opt(a.attgenerated)),
        ("atttypmod", opt(a.atttypmod)),
        ("attidentity", opt(a.attidentity)),
        (
            "attcollation",
            n.coll(a.attcollation.map_or(0, |c| c.get())),
        ),
        ("attislocal", b(a.attislocal)),
        ("attinhcount", a.attinhcount.to_string()),
    ])
}

/// Every difference between the analyzer's dump and the server's, one line
/// each, in table and identity order.
pub(crate) fn diff(ours: &Dump, pg: &Dump) -> Vec<String> {
    let mut out = Vec::new();
    let keys: BTreeSet<&(&'static str, String)> = ours.0.keys().chain(pg.0.keys()).collect();
    for key in keys {
        let (table, identity) = key;
        match (ours.0.get(key), pg.0.get(key)) {
            (Some(_), None) => out.push(format!("{table} {identity}: only in the analyzer")),
            (None, Some(_)) => out.push(format!("{table} {identity}: only in PostgreSQL")),
            (Some(a), Some(p)) => {
                for (field, ours) in a {
                    let theirs = p.get(field).map(String::as_str).unwrap_or("<absent>");
                    if ours != theirs {
                        out.push(format!(
                            "{table} {identity}: {field} is {ours} in the analyzer, {theirs} in PostgreSQL"
                        ));
                    }
                }
            }
            (None, None) => unreachable!(),
        }
    }
    out
}
