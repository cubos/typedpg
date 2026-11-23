//! The catalog oracle's second layer: the catalogs and columns the seed
//! rows ([`crate::PgCatalogSeed`]) don't carry, which the analyzer keeps in
//! side tables — `pg_class` flags, `pg_trigger`, `pg_policy`,
//! `pg_sequence`, constraint deferrability and foreign-key actions, domain
//! constraints, `pg_index` flags and access methods, index column names,
//! `pg_partitioned_table`, rules, and column storage / compression.
//!
//! Each side is read into [`Extras`], OIDs still raw; `catalog_state`
//! names them like the seed rows' and compares them.

use postgres::Client;

use crate::pg_catalog::{PgCatalog, RelKind};

/// `pg_class` columns the seed's `PgClass` lacks.
pub(crate) struct XClass {
    pub(crate) relid: u32,
    pub(crate) persistence: char,
    pub(crate) rowsecurity: bool,
    pub(crate) forcerowsecurity: bool,
    pub(crate) replident: char,
    pub(crate) ispartition: bool,
    pub(crate) oftype: u32,
    pub(crate) hastoast: bool,
}

/// A user trigger (`pg_trigger`, not `tgisinternal`).
pub(crate) struct XTrigger {
    pub(crate) relid: u32,
    pub(crate) name: String,
    pub(crate) function: u32,
    pub(crate) tgtype: i16,
    pub(crate) enabled: char,
    pub(crate) columns: Vec<String>,
    pub(crate) constrrel: u32,
    /// The relation of the trigger it clones (`tgparentid`'s).
    pub(crate) parent_rel: u32,
    pub(crate) constraint: bool,
    pub(crate) deferrable: bool,
    pub(crate) initdeferred: bool,
    pub(crate) has_when: bool,
    pub(crate) old_table: String,
    pub(crate) new_table: String,
}

/// `pg_policy`.
pub(crate) struct XPolicy {
    pub(crate) relid: u32,
    pub(crate) name: String,
    pub(crate) cmd: char,
    pub(crate) permissive: bool,
    pub(crate) roles: Vec<String>,
}

/// `pg_sequence`.
pub(crate) struct XSequence {
    pub(crate) relid: u32,
    pub(crate) typ: u32,
    pub(crate) start: i64,
    pub(crate) increment: i64,
    pub(crate) max: i64,
    pub(crate) min: i64,
    pub(crate) cache: i64,
}

/// A table constraint's deferrability and, for a foreign key, actions.
pub(crate) struct XConstraint {
    pub(crate) oid: u32,
    pub(crate) deferrable: bool,
    /// `None` where the analyzer doesn't keep it (an index constraint's).
    pub(crate) initdeferred: Option<bool>,
    /// `confupdtype`, `confdeltype`, `confmatchtype` of a foreign key.
    pub(crate) fk: Option<(char, char, char)>,
}

/// A domain constraint (`pg_constraint.contypid`).
pub(crate) struct XDomainConstraint {
    pub(crate) typid: u32,
    pub(crate) name: String,
    pub(crate) contype: char,
}

/// `pg_index` flags, the access method and the index's column names.
pub(crate) struct XIndex {
    pub(crate) indexrelid: u32,
    pub(crate) am: String,
    pub(crate) valid: bool,
    pub(crate) immediate: bool,
    pub(crate) clustered: bool,
    pub(crate) replident: bool,
    pub(crate) nulls_not_distinct: bool,
    pub(crate) columns: Vec<String>,
}

/// `pg_partitioned_table`.
pub(crate) struct XPartitioned {
    pub(crate) relid: u32,
    pub(crate) strategy: char,
    pub(crate) attnums: Vec<i16>,
}

/// A rule other than a view's `_RETURN` (`pg_rewrite`).
pub(crate) struct XRule {
    pub(crate) relid: u32,
    pub(crate) name: String,
    pub(crate) event: char,
    pub(crate) instead: bool,
    /// `ev_enabled` is `O` or `A`: it fires in an origin session.
    pub(crate) fires: bool,
}

/// A column's `attstorage` and `attcompression`.
pub(crate) struct XAttribute {
    pub(crate) relid: u32,
    pub(crate) attnum: i16,
    pub(crate) storage: char,
    pub(crate) compression: char,
}

#[derive(Default)]
pub(crate) struct Extras {
    pub(crate) classes: Vec<XClass>,
    pub(crate) triggers: Vec<XTrigger>,
    pub(crate) policies: Vec<XPolicy>,
    pub(crate) sequences: Vec<XSequence>,
    pub(crate) constraints: Vec<XConstraint>,
    pub(crate) domain_constraints: Vec<XDomainConstraint>,
    pub(crate) indexes: Vec<XIndex>,
    pub(crate) partitioned: Vec<XPartitioned>,
    pub(crate) rules: Vec<XRule>,
    pub(crate) attributes: Vec<XAttribute>,
}

const FIRST_NORMAL: u32 = crate::catalog_export::FIRST_NORMAL_OBJECT_ID;

fn ch(v: i8) -> char {
    v as u8 as char
}

/// The server's extras, of the objects initdb didn't create.
pub(crate) fn from_server(client: &mut Client) -> Result<Extras, postgres::Error> {
    let mut x = Extras::default();
    for r in client.query(
        "SELECT oid, relpersistence, relrowsecurity, relforcerowsecurity, relreplident, \
                relispartition, reloftype, reltoastrelid <> 0 \
         FROM pg_catalog.pg_class \
         WHERE oid >= $1 AND relkind IN ('r', 'p', 'v', 'm', 'f', 'S', 'c')",
        &[&FIRST_NORMAL],
    )? {
        x.classes.push(XClass {
            relid: r.get(0),
            persistence: ch(r.get(1)),
            rowsecurity: r.get(2),
            forcerowsecurity: r.get(3),
            replident: ch(r.get(4)),
            ispartition: r.get(5),
            oftype: r.get(6),
            hastoast: r.get(7),
        });
    }
    for r in client.query(
        "SELECT t.tgrelid, t.tgname::text, t.tgfoid, t.tgtype, t.tgenabled, \
                ARRAY(SELECT a.attname::text \
                      FROM pg_catalog.unnest(t.tgattr::int2[]) WITH ORDINALITY u(n, i) \
                      JOIN pg_catalog.pg_attribute a \
                        ON a.attrelid = t.tgrelid AND a.attnum = u.n \
                      ORDER BY u.i), \
                t.tgconstrrelid, coalesce(p.tgrelid, 0::oid), t.tgconstraint <> 0, \
                t.tgdeferrable, t.tginitdeferred, t.tgqual IS NOT NULL, \
                coalesce(t.tgoldtable::text, ''), coalesce(t.tgnewtable::text, '') \
         FROM pg_catalog.pg_trigger t \
         LEFT JOIN pg_catalog.pg_trigger p ON p.oid = t.tgparentid \
         WHERE NOT t.tgisinternal AND t.tgrelid >= $1",
        &[&FIRST_NORMAL],
    )? {
        x.triggers.push(XTrigger {
            relid: r.get(0),
            name: r.get(1),
            function: r.get(2),
            tgtype: r.get(3),
            enabled: ch(r.get(4)),
            columns: r.get(5),
            constrrel: r.get(6),
            parent_rel: r.get(7),
            constraint: r.get(8),
            deferrable: r.get(9),
            initdeferred: r.get(10),
            has_when: r.get(11),
            old_table: r.get(12),
            new_table: r.get(13),
        });
    }
    for r in client.query(
        "SELECT polrelid, polname::text, polcmd, polpermissive, \
                ARRAY(SELECT CASE WHEN o = 0 THEN 'public' \
                                  ELSE pg_catalog.pg_get_userbyid(o)::text END \
                      FROM pg_catalog.unnest(polroles) o ORDER BY 1) \
         FROM pg_catalog.pg_policy WHERE polrelid >= $1",
        &[&FIRST_NORMAL],
    )? {
        x.policies.push(XPolicy {
            relid: r.get(0),
            name: r.get(1),
            cmd: ch(r.get(2)),
            permissive: r.get(3),
            roles: r.get(4),
        });
    }
    for r in client.query(
        "SELECT seqrelid, seqtypid, seqstart, seqincrement, seqmax, seqmin, seqcache \
         FROM pg_catalog.pg_sequence WHERE seqrelid >= $1",
        &[&FIRST_NORMAL],
    )? {
        x.sequences.push(XSequence {
            relid: r.get(0),
            typ: r.get(1),
            start: r.get(2),
            increment: r.get(3),
            max: r.get(4),
            min: r.get(5),
            cache: r.get(6),
        });
    }
    for r in client.query(
        "SELECT oid, contype, condeferrable, condeferred, confupdtype, confdeltype, \
                confmatchtype \
         FROM pg_catalog.pg_constraint \
         WHERE oid >= $1 AND conrelid <> 0 AND contype IN ('f', 'p', 'u', 'x', 't')",
        &[&FIRST_NORMAL],
    )? {
        let contype = ch(r.get(1));
        x.constraints.push(XConstraint {
            oid: r.get(0),
            deferrable: r.get(2),
            initdeferred: (!matches!(contype, 'p' | 'u' | 'x')).then(|| r.get(3)),
            fk: (contype == 'f').then(|| (ch(r.get(4)), ch(r.get(5)), ch(r.get(6)))),
        });
    }
    for r in client.query(
        "SELECT contypid, conname::text, contype FROM pg_catalog.pg_constraint \
         WHERE contypid >= $1",
        &[&FIRST_NORMAL],
    )? {
        x.domain_constraints.push(XDomainConstraint {
            typid: r.get(0),
            name: r.get(1),
            contype: ch(r.get(2)),
        });
    }
    for r in client.query(
        "SELECT i.indexrelid, a.amname::text, i.indisvalid, i.indimmediate, i.indisclustered, \
                i.indisreplident, i.indnullsnotdistinct, \
                ARRAY(SELECT t.attname::text FROM pg_catalog.pg_attribute t \
                      WHERE t.attrelid = i.indexrelid AND t.attnum > 0 ORDER BY t.attnum) \
         FROM pg_catalog.pg_index i \
         JOIN pg_catalog.pg_class c ON c.oid = i.indexrelid \
         JOIN pg_catalog.pg_am a ON a.oid = c.relam \
         WHERE i.indexrelid >= $1 AND i.indrelid >= $1",
        &[&FIRST_NORMAL],
    )? {
        x.indexes.push(XIndex {
            indexrelid: r.get(0),
            am: r.get(1),
            valid: r.get(2),
            immediate: r.get(3),
            clustered: r.get(4),
            replident: r.get(5),
            nulls_not_distinct: r.get(6),
            columns: r.get(7),
        });
    }
    for r in client.query(
        "SELECT partrelid, partstrat, partattrs::int2[] FROM pg_catalog.pg_partitioned_table \
         WHERE partrelid >= $1",
        &[&FIRST_NORMAL],
    )? {
        x.partitioned.push(XPartitioned {
            relid: r.get(0),
            strategy: ch(r.get(1)),
            attnums: r.get(2),
        });
    }
    for r in client.query(
        "SELECT ev_class, rulename::text, ev_type, is_instead, ev_enabled IN ('O', 'A') \
         FROM pg_catalog.pg_rewrite WHERE ev_class >= $1 AND rulename <> '_RETURN'",
        &[&FIRST_NORMAL],
    )? {
        x.rules.push(XRule {
            relid: r.get(0),
            name: r.get(1),
            event: ch(r.get(2)),
            instead: r.get(3),
            fires: r.get(4),
        });
    }
    for r in client.query(
        "SELECT a.attrelid, a.attnum, a.attstorage, a.attcompression \
         FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
         WHERE a.attrelid >= $1 AND a.attnum > 0 AND NOT a.attisdropped \
           AND c.relkind IN ('r', 'p', 'm')",
        &[&FIRST_NORMAL],
    )? {
        x.attributes.push(XAttribute {
            relid: r.get(0),
            attnum: r.get(1),
            storage: ch(r.get(2)),
            compression: ch(r.get(3)),
        });
    }
    Ok(x)
}

/// The analyzer's extras, of its user objects; `role_name` names a role
/// the way the server does.
pub(crate) fn from_model(
    db: &PgCatalog,
    role_name: &dyn Fn(crate::ddl::roles::RoleId) -> String,
) -> Extras {
    use crate::ddl::policies::PolicyRole;
    use crate::ddl::triggers::TriggerState;
    use crate::pg_catalog::ConType;
    let user = |oid: u32| oid >= FIRST_NORMAL;
    let mut x = Extras::default();
    for c in db.pg_class.values().filter(|c| user(c.oid.get())) {
        if !matches!(
            c.relkind,
            RelKind::Table
                | RelKind::Partitioned
                | RelKind::View
                | RelKind::MaterializedView
                | RelKind::ForeignTable
                | RelKind::Sequence
                | RelKind::CompositeType
        ) {
            continue;
        }
        x.classes.push(XClass {
            relid: c.oid.get(),
            persistence: db.relpersistence.get(&c.oid).copied().unwrap_or('p'),
            rowsecurity: db.row_security.contains(&c.oid),
            forcerowsecurity: db.force_row_security.contains(&c.oid),
            // heap_create_with_catalog: DEFAULT for a table or a
            // materialized view, NOTHING for the other relation kinds.
            replident: if db.replica_identity_indexes.contains_key(&c.oid) {
                'i'
            } else {
                db.replica_identity_kinds
                    .get(&c.oid)
                    .copied()
                    .unwrap_or(match c.relkind {
                        RelKind::Table | RelKind::Partitioned | RelKind::MaterializedView => 'd',
                        _ => 'n',
                    })
            },
            ispartition: db.partition_bounds.contains_key(&c.oid),
            oftype: db.typed_tables.get(&c.oid).map_or(0, |t| t.get()),
            // `toast_tables` is brought up to date lazily (before a
            // statement that reads it: `note_toast_tables`): what it would
            // hold now.
            hastoast: crate::ddl::tables::toast::has_toast_table_now(db, c.oid),
        });
    }
    for (relid, triggers) in db.triggers.iter() {
        for t in triggers {
            let state = match t.state {
                TriggerState::Origin => 'O',
                TriggerState::Always => 'A',
                TriggerState::Replica => 'R',
                TriggerState::Disabled => 'D',
            };
            let table = |is_new: bool| {
                t.transition_tables
                    .iter()
                    .find(|(_, n)| *n == is_new)
                    .map_or_else(String::new, |(name, _)| name.clone())
            };
            x.triggers.push(XTrigger {
                relid: relid.get(),
                name: t.name.clone(),
                function: t.function.get(),
                tgtype: (i32::from(t.row) | t.timing | t.events) as i16,
                enabled: state,
                columns: t.columns.clone(),
                constrrel: t.constrrel.map_or(0, |r| r.get()),
                parent_rel: t.parent.map_or(0, |r| r.get()),
                constraint: t.constraint_deferrable.is_some(),
                deferrable: t.constraint_deferrable.unwrap_or(false),
                initdeferred: t.constraint_deferred.unwrap_or(false),
                has_when: t.when.is_some(),
                old_table: table(false),
                new_table: table(true),
            });
        }
    }
    for (relid, policies) in db.policies.iter() {
        for p in policies {
            let mut roles: Vec<String> = p
                .roles
                .iter()
                .map(|r| match r {
                    PolicyRole::Public => "public".to_owned(),
                    PolicyRole::Role(id) => role_name(*id),
                })
                .collect();
            roles.sort();
            x.policies.push(XPolicy {
                relid: relid.get(),
                name: p.name.clone(),
                cmd: p.cmd,
                permissive: p.permissive,
                roles,
            });
        }
    }
    for (relid, s) in db.sequence_params.iter() {
        x.sequences.push(XSequence {
            relid: relid.get(),
            typ: s.typ.get(),
            start: s.start as i64,
            increment: s.increment as i64,
            max: s.max as i64,
            min: s.min as i64,
            cache: s.cache as i64,
        });
    }
    for c in db.pg_constraint.values().filter(|c| user(c.oid.get())) {
        let (deferrable, initdeferred, fk) = match c.contype {
            ConType::ForeignKey => {
                let d = db.fk_details.get(&c.oid);
                (
                    d.is_some_and(|d| d.deferrable()),
                    Some(d.is_some_and(|d| d.initdeferred())),
                    d.map(|d| d.catalog_codes()),
                )
            }
            ConType::PrimaryKey | ConType::Unique | ConType::Exclusion => {
                let ns = db.pg_class.get(&c.conrelid).map(|r| r.relnamespace);
                let deferrable = ns
                    .and_then(|ns| db.class_by_qname.get(&(ns, c.conname.clone())))
                    .is_some_and(|index| db.nonimmediate_indexes.contains(index));
                (deferrable, None, None)
            }
            ConType::Trigger => {
                let t = db
                    .triggers
                    .get(&c.conrelid)
                    .into_iter()
                    .flatten()
                    .find(|t| t.constraint == Some(c.oid));
                (
                    t.and_then(|t| t.constraint_deferrable).unwrap_or(false),
                    Some(t.and_then(|t| t.constraint_deferred).unwrap_or(false)),
                    None,
                )
            }
            _ => continue,
        };
        x.constraints.push(XConstraint {
            oid: c.oid.get(),
            deferrable,
            initdeferred,
            fk,
        });
    }
    for (typid, constraints) in db.domain_constraints.iter() {
        for c in constraints {
            x.domain_constraints.push(XDomainConstraint {
                typid: typid.get(),
                name: c.name.clone(),
                contype: match c.kind {
                    crate::ddl::types::DomainConstraintKind::NotNull => 'n',
                    crate::ddl::types::DomainConstraintKind::Check => 'c',
                },
            });
        }
    }
    for i in db
        .pg_index
        .values()
        .filter(|i| user(i.indexrelid.get()) && user(i.indrelid.get()))
    {
        let oid = i.indexrelid;
        x.indexes.push(XIndex {
            indexrelid: oid.get(),
            am: db
                .index_access_methods
                .get(&oid)
                .cloned()
                .unwrap_or_else(|| "btree".to_owned()),
            valid: !db.invalid_indexes.contains(&oid),
            immediate: !db.nonimmediate_indexes.contains(&oid),
            clustered: db.clustered_indexes.values().any(|&c| c == oid),
            replident: db.replica_identity_indexes.values().any(|&c| c == oid),
            nulls_not_distinct: db
                .index_keys
                .get(&oid)
                .is_some_and(|k| k.nulls_not_distinct),
            columns: crate::ddl::indexes::index_attnames(db, oid),
        });
    }
    for (relid, spec) in db.partition_specs.iter() {
        x.partitioned.push(XPartitioned {
            relid: relid.get(),
            strategy: spec.strategy_char(),
            attnums: db.partition_keys.get(relid).cloned().unwrap_or_default(),
        });
    }
    for (relid, rules) in db.rules.iter() {
        for r in rules {
            use typedpg_pg_query::ast::CmdType;
            x.rules.push(XRule {
                relid: relid.get(),
                name: r.name.clone(),
                event: match r.event {
                    CmdType::CmdSelect => '1',
                    CmdType::CmdUpdate => '2',
                    CmdType::CmdInsert => '3',
                    CmdType::CmdDelete => '4',
                    _ => '?',
                },
                instead: r.instead,
                fires: r.enabled,
            });
        }
    }
    for (relid, attrs) in db.pg_attribute.iter() {
        let kind = db.pg_class.get(relid).map(|c| c.relkind);
        if !user(relid.get())
            || !matches!(
                kind,
                Some(RelKind::Table | RelKind::Partitioned | RelKind::MaterializedView)
            )
        {
            continue;
        }
        for a in attrs.iter().filter(|a| a.attnum > 0) {
            let storage = db
                .attr_storage
                .get(&(*relid, a.attnum))
                .copied()
                .or_else(|| db.pg_type.get(&a.atttypid).map(|t| t.typstorage))
                .map_or('?', |s| s.as_char() as char);
            let compression = match db.attr_compression.get(&(*relid, a.attnum)) {
                Some(&"pglz") => 'p',
                Some(&"lz4") => 'l',
                _ => '\0',
            };
            x.attributes.push(XAttribute {
                relid: relid.get(),
                attnum: a.attnum,
                storage,
                compression,
            });
        }
    }
    x
}
