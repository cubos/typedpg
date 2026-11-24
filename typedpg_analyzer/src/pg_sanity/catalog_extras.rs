//! The catalog oracle's second layer: the catalogs and columns the seed
//! rows ([`crate::PgCatalogSeed`]) don't carry, which the analyzer keeps in
//! side tables — `pg_class` flags, `pg_trigger`, `pg_policy`,
//! `pg_sequence`, constraint deferrability and foreign-key actions, domain
//! constraints, `pg_index` flags and access methods, index column names,
//! `pg_partitioned_table`, rules, column storage / compression, extended
//! statistics and publications — and the identities of the objects of
//! catalogs the seed rows don't cover that `pg_depend` rows name.
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
    pub(crate) cycle: bool,
}

/// `pg_publication`.
pub(crate) struct XPublication {
    pub(crate) name: String,
    pub(crate) alltables: bool,
    pub(crate) allsequences: bool,
    pub(crate) viaroot: bool,
    /// `pubinsert`, `pubupdate`, `pubdelete`, `pubtruncate`.
    pub(crate) publish: [bool; 4],
    pub(crate) gencols: char,
    /// `pg_publication_rel`: table, row filter, column list, `prexcept`.
    pub(crate) relations: Vec<(u32, bool, bool, bool)>,
    /// `pg_publication_namespace`.
    pub(crate) schemas: Vec<u32>,
}

/// `pg_statistic_ext`.
pub(crate) struct XStatistics {
    pub(crate) nsp: u32,
    pub(crate) name: String,
    pub(crate) relid: u32,
    pub(crate) keys: Vec<i16>,
    pub(crate) kinds: Vec<char>,
    pub(crate) exprs: usize,
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

/// An object of a catalog the seed rows don't cover, which `pg_depend`
/// rows name (by `(classid, objid)`): its identity, OIDs still raw.
#[derive(Clone, Debug)]
pub(crate) enum DepObject {
    /// A column's `pg_attrdef` row (its default or generation expression).
    Attrdef {
        relid: u32,
        attnum: i16,
    },
    Rule {
        relid: u32,
        name: String,
    },
    Trigger {
        relid: u32,
        name: String,
    },
    /// An internal trigger (a foreign key's): named after its OID, so
    /// identified by its constraint, relation and function.
    InternalTrigger {
        relid: u32,
        constraint: u32,
        function: u32,
    },
    Policy {
        relid: u32,
        name: String,
    },
    Statistics {
        nsp: u32,
        name: String,
    },
    Publication(String),
    PublicationRel {
        publication: String,
        relid: u32,
    },
    PublicationNamespace {
        publication: String,
        nsp: u32,
    },
    DomainConstraint {
        typid: u32,
        name: String,
    },
    EventTrigger(String),
    Server(String),
    Subscription(String),
    UserMapping {
        user: String,
        server: String,
    },
    /// A `pg_amop` (`func` false) or `pg_amproc` row of a family.
    FamilyMember {
        func: bool,
        family: u32,
        number: i16,
        left: u32,
        right: u32,
    },
    Transform {
        typ: u32,
        lang: String,
    },
    Conversion {
        nsp: u32,
        name: String,
    },
    DefaultAcl {
        role: String,
        nsp: u32,
        objtype: char,
    },
}

#[derive(Default)]
pub(crate) struct Extras {
    /// The objects of the catalogs the seed rows don't cover, by
    /// `(classid, objid)`.
    pub(crate) dep_objects: std::collections::HashMap<(u32, u32), DepObject>,
    pub(crate) classes: Vec<XClass>,
    pub(crate) triggers: Vec<XTrigger>,
    pub(crate) policies: Vec<XPolicy>,
    pub(crate) sequences: Vec<XSequence>,
    pub(crate) statistics: Vec<XStatistics>,
    pub(crate) publications: Vec<XPublication>,
    pub(crate) constraints: Vec<XConstraint>,
    pub(crate) domain_constraints: Vec<XDomainConstraint>,
    pub(crate) indexes: Vec<XIndex>,
    pub(crate) partitioned: Vec<XPartitioned>,
    pub(crate) rules: Vec<XRule>,
    pub(crate) attributes: Vec<XAttribute>,
    /// The `pg_depend` rows of kind `P` (`true`) or `S`, by `(classid,
    /// objid, objsubid, refclassid, refobjid, refobjsubid)`: the seed rows
    /// carry them as `Normal`.
    pub(crate) partition_deps: std::collections::HashMap<(u32, u32, i16, u32, u32, i16), bool>,
}

const FIRST_NORMAL: u32 = crate::catalog_export::FIRST_NORMAL_OBJECT_ID;

fn ch(v: i8) -> char {
    v as u8 as char
}

/// The server's extras, of the objects initdb didn't create.
pub(crate) fn from_server(client: &mut Client) -> Result<Extras, postgres::Error> {
    let mut x = Extras::default();
    for r in client.query(
        "SELECT classid::int4, objid::int4, objsubid, refclassid::int4, refobjid::int4, \
                refobjsubid, deptype = 'P' \
         FROM pg_catalog.pg_depend \
         WHERE deptype IN ('P', 'S') AND (objid >= $1 OR refobjid >= $1)",
        &[&FIRST_NORMAL],
    )? {
        let oid = |i: usize| r.get::<_, i32>(i) as u32;
        x.partition_deps.insert(
            (
                oid(0),
                oid(1),
                r.get::<_, i32>(2) as i16,
                oid(3),
                oid(4),
                r.get::<_, i32>(5) as i16,
            ),
            r.get(6),
        );
    }
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
        "SELECT seqrelid, seqtypid, seqstart, seqincrement, seqmax, seqmin, seqcache, \
                seqcycle \
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
            cycle: r.get(7),
        });
    }
    for r in client.query(
        "SELECT stxnamespace, stxname::text, stxrelid, stxkeys::int2[], stxkind::text[], \
                coalesce(cardinality(pg_catalog.pg_get_statisticsobjdef_expressions(oid)), 0) \
         FROM pg_catalog.pg_statistic_ext WHERE oid >= $1",
        &[&FIRST_NORMAL],
    )? {
        let kinds: Vec<String> = r.get(4);
        x.statistics.push(XStatistics {
            nsp: r.get(0),
            name: r.get(1),
            relid: r.get(2),
            keys: r.get(3),
            kinds: kinds.iter().filter_map(|k| k.chars().next()).collect(),
            exprs: r.get::<_, i32>(5) as usize,
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
    for r in client.query(
        "SELECT p.pubname::text, p.puballtables, p.puballsequences, p.pubviaroot, \
                p.pubinsert, p.pubupdate, p.pubdelete, p.pubtruncate, p.pubgencols, \
                ARRAY(SELECT u.v FROM pg_catalog.pg_publication_rel r, \
                        LATERAL unnest(ARRAY[r.prrelid::int8, \
                                             (r.prqual IS NOT NULL)::int::int8, \
                                             (r.prattrs IS NOT NULL)::int::int8, \
                                             r.prexcept::int::int8]) WITH ORDINALITY u(v, i) \
                      WHERE r.prpubid = p.oid ORDER BY r.oid, u.i), \
                ARRAY(SELECT n.pnnspid FROM pg_catalog.pg_publication_namespace n \
                      WHERE n.pnpubid = p.oid) \
         FROM pg_catalog.pg_publication p",
        &[],
    )? {
        let flat: Vec<i64> = r.get(9);
        let relations: Vec<&[i64]> = flat.chunks(4).collect();
        x.publications.push(XPublication {
            name: r.get(0),
            alltables: r.get(1),
            allsequences: r.get(2),
            viaroot: r.get(3),
            publish: [r.get(4), r.get(5), r.get(6), r.get(7)],
            gencols: ch(r.get(8)),
            relations: relations
                .iter()
                .map(|v| (v[0] as u32, v[1] != 0, v[2] != 0, v[3] != 0))
                .collect(),
            schemas: r.get(10),
        });
    }
    server_dep_objects(client, &mut x)?;
    Ok(x)
}

/// The identities of the server's objects `pg_depend` rows may name in the
/// catalogs the seed rows don't cover.
fn server_dep_objects(client: &mut Client, x: &mut Extras) -> Result<(), postgres::Error> {
    let mut put = |classid: u32, oid: u32, object: DepObject| {
        x.dep_objects.insert((classid, oid), object);
    };
    for r in client.query(
        "SELECT oid, adrelid, adnum FROM pg_catalog.pg_attrdef WHERE adrelid >= $1",
        &[&FIRST_NORMAL],
    )? {
        put(
            2604,
            r.get(0),
            DepObject::Attrdef {
                relid: r.get(1),
                attnum: r.get(2),
            },
        );
    }
    for r in client.query(
        "SELECT oid, ev_class, rulename::text FROM pg_catalog.pg_rewrite WHERE ev_class >= $1",
        &[&FIRST_NORMAL],
    )? {
        put(
            2618,
            r.get(0),
            DepObject::Rule {
                relid: r.get(1),
                name: r.get(2),
            },
        );
    }
    for r in client.query(
        "SELECT oid, tgrelid, tgname::text, tgisinternal, tgconstraint, tgfoid \
         FROM pg_catalog.pg_trigger WHERE oid >= $1",
        &[&FIRST_NORMAL],
    )? {
        let object = if r.get::<_, bool>(3) {
            DepObject::InternalTrigger {
                relid: r.get(1),
                constraint: r.get(4),
                function: r.get(5),
            }
        } else {
            DepObject::Trigger {
                relid: r.get(1),
                name: r.get(2),
            }
        };
        put(2620, r.get(0), object);
    }
    for r in client.query(
        "SELECT oid, polrelid, polname::text FROM pg_catalog.pg_policy WHERE polrelid >= $1",
        &[&FIRST_NORMAL],
    )? {
        put(
            3256,
            r.get(0),
            DepObject::Policy {
                relid: r.get(1),
                name: r.get(2),
            },
        );
    }
    for r in client.query(
        "SELECT oid, stxnamespace, stxname::text FROM pg_catalog.pg_statistic_ext \
         WHERE oid >= $1",
        &[&FIRST_NORMAL],
    )? {
        put(
            3381,
            r.get(0),
            DepObject::Statistics {
                nsp: r.get(1),
                name: r.get(2),
            },
        );
    }
    for r in client.query(
        "SELECT oid, pubname::text FROM pg_catalog.pg_publication",
        &[],
    )? {
        put(6104, r.get(0), DepObject::Publication(r.get(1)));
    }
    for r in client.query(
        "SELECT r.oid, p.pubname::text, r.prrelid FROM pg_catalog.pg_publication_rel r \
         JOIN pg_catalog.pg_publication p ON p.oid = r.prpubid",
        &[],
    )? {
        put(
            6106,
            r.get(0),
            DepObject::PublicationRel {
                publication: r.get(1),
                relid: r.get(2),
            },
        );
    }
    for r in client.query(
        "SELECT n.oid, p.pubname::text, n.pnnspid FROM pg_catalog.pg_publication_namespace n \
         JOIN pg_catalog.pg_publication p ON p.oid = n.pnpubid",
        &[],
    )? {
        put(
            6237,
            r.get(0),
            DepObject::PublicationNamespace {
                publication: r.get(1),
                nsp: r.get(2),
            },
        );
    }
    for r in client.query(
        "SELECT oid, contypid, conname::text FROM pg_catalog.pg_constraint \
         WHERE contypid >= $1",
        &[&FIRST_NORMAL],
    )? {
        put(
            2606,
            r.get(0),
            DepObject::DomainConstraint {
                typid: r.get(1),
                name: r.get(2),
            },
        );
    }
    // A built-in conversion moved to a schema of the migrations' (its OID
    // the seed's, its schema not).
    for r in client.query(
        "SELECT oid, connamespace, conname::text FROM pg_catalog.pg_conversion \
         WHERE oid < $1 AND connamespace >= $1",
        &[&FIRST_NORMAL],
    )? {
        put(
            2607,
            r.get(0),
            DepObject::Conversion {
                nsp: r.get(1),
                name: r.get(2),
            },
        );
    }
    for r in client.query(
        "SELECT oid, evtname::text FROM pg_catalog.pg_event_trigger",
        &[],
    )? {
        put(3466, r.get(0), DepObject::EventTrigger(r.get(1)));
    }
    for r in client.query(
        "SELECT oid, subname::text FROM pg_catalog.pg_subscription",
        &[],
    )? {
        put(6100, r.get(0), DepObject::Subscription(r.get(1)));
    }
    for r in client.query(
        "SELECT oid, srvname::text FROM pg_catalog.pg_foreign_server",
        &[],
    )? {
        put(1417, r.get(0), DepObject::Server(r.get(1)));
    }
    // The view, not `pg_user_mapping` (which only a superuser reads: the
    // session may be under SET ROLE).
    for r in client.query(
        "SELECT umid, CASE WHEN umuser = 0 THEN 'public' \
                            ELSE pg_catalog.pg_get_userbyid(umuser)::text END, \
                srvname::text \
         FROM pg_catalog.pg_user_mappings",
        &[],
    )? {
        put(
            1418,
            r.get(0),
            DepObject::UserMapping {
                user: r.get(1),
                server: r.get(2),
            },
        );
    }
    for r in client.query(
        "SELECT oid, amopfamily, amopstrategy, amoplefttype, amoprighttype \
         FROM pg_catalog.pg_amop WHERE oid >= $1",
        &[&FIRST_NORMAL],
    )? {
        put(
            2602,
            r.get(0),
            DepObject::FamilyMember {
                func: false,
                family: r.get(1),
                number: r.get(2),
                left: r.get(3),
                right: r.get(4),
            },
        );
    }
    for r in client.query(
        "SELECT oid, amprocfamily, amprocnum, amproclefttype, amprocrighttype \
         FROM pg_catalog.pg_amproc WHERE oid >= $1",
        &[&FIRST_NORMAL],
    )? {
        put(
            2603,
            r.get(0),
            DepObject::FamilyMember {
                func: true,
                family: r.get(1),
                number: r.get(2),
                left: r.get(3),
                right: r.get(4),
            },
        );
    }
    for r in client.query(
        "SELECT t.oid, t.trftype, l.lanname::text FROM pg_catalog.pg_transform t \
         JOIN pg_catalog.pg_language l ON l.oid = t.trflang",
        &[],
    )? {
        put(
            3576,
            r.get(0),
            DepObject::Transform {
                typ: r.get(1),
                lang: r.get(2),
            },
        );
    }
    for r in client.query(
        "SELECT oid, pg_catalog.pg_get_userbyid(defaclrole)::text, defaclnamespace, \
                defaclobjtype FROM pg_catalog.pg_default_acl",
        &[],
    )? {
        put(
            826,
            r.get(0),
            DepObject::DefaultAcl {
                role: r.get(1),
                nsp: r.get(2),
                objtype: ch(r.get(3)),
            },
        );
    }
    Ok(())
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
            cycle: s.cycle,
        });
    }
    for p in db.publications.iter() {
        let (alltables, allsequences, viaroot, publish, gencols) = p.flags();
        x.publications.push(XPublication {
            name: p.name.clone(),
            alltables,
            allsequences,
            viaroot,
            publish,
            gencols,
            relations: p
                .relation_rows()
                .into_iter()
                .map(|(rel, q, c, e)| (rel.get(), q, c, e))
                .collect(),
            schemas: p.members().1.iter().map(|n| n.get()).collect(),
        });
    }
    for s in db.statistics.iter() {
        x.statistics.push(XStatistics {
            nsp: s.namespace.get(),
            name: s.name.clone(),
            relid: s.relid.get(),
            keys: s.keys.clone(),
            kinds: s.kinds.clone(),
            exprs: s.exprs,
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
