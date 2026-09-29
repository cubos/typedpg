//! Generates `seed.json` for the static analyzer from a live PostgreSQL instance.
//!
//! Usage:
//!   cargo run -p typedpg_seed
//!
//! Spins up a disposable `postgres:latest` container via the Docker daemon
//! (using `testcontainers`), waits for it to accept connections, exports each
//! `pg_catalog` table almost 1:1 into the analyzer's `PgCatalogSeed`, then
//! stops + removes the container (via `Drop`). The output is written to
//! `typedpg_analyzer/src/seed.json`.

use std::collections::HashMap;

use testcontainers::ImageExt;
use testcontainers::core::Mount;
use testcontainers::runners::SyncRunner;
use testcontainers_modules::postgres::Postgres;
use typedpg_analyzer::{
    AggKind, ArgMode, AttGenerated, AttIdentity, CastContext, CastMethod, ConType, DepType,
    PgAggregate, PgAm, PgAttribute, PgCast, PgCastOid, PgCatalog, PgCatalogSeed, PgClass,
    PgClassOid, PgCollation, PgCollationOid, PgConstraint, PgConstraintOid, PgDepend, PgEnum,
    PgEnumOid, PgExtension, PgExtensionOid, PgGenericOid, PgIndex, PgInherits, PgNamespace,
    PgNamespaceOid, PgOpclass, PgOperator, PgOperatorOid, PgOpfamily, PgProc, PgProcOid, PgRange,
    PgSetting, PgTsObject, PgType, PgTypeOid, ProKind, ProVolatile, QualifiedName, RelKind,
    TypCategory, TypStorage, TypType,
};

fn main() {
    eprintln!("Pulling postgres:latest from registry...");
    let request = Postgres::default()
        .with_tag("latest")
        .with_mount(Mount::tmpfs_mount("/var/lib/postgresql"))
        .pull_image()
        .expect("failed to pull postgres:latest");

    eprintln!("Starting postgres:latest container...");
    let container = request.start().expect("failed to start postgres container");

    let host = container.get_host().expect("failed to get container host");
    let port = container
        .get_host_port_ipv4(5432)
        .expect("failed to get container port");
    let conn_str =
        format!("host={host} port={port} user=postgres password=postgres dbname=postgres");

    eprintln!("Connecting to: {conn_str}");
    let mut client = postgres::Client::connect(&conn_str, postgres::NoTls)
        .expect("failed to connect to PostgreSQL");

    eprintln!("Exporting catalog...");
    let snapshot = export_catalog(&mut client).expect("failed to export schema");

    eprintln!("Exporting view definitions...");
    let view_defs =
        export_view_definitions(&mut client).expect("failed to export view definitions");
    eprintln!(
        "Populating pg_rewrite._RETURN for {} view(s)...",
        view_defs.len()
    );
    let snapshot = populate_view_defs(snapshot, view_defs);

    let json = serde_json::to_string(&snapshot).expect("failed to serialize snapshot");

    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let out_path = std::path::Path::new(manifest_dir)
        .parent()
        .unwrap()
        .join("typedpg_analyzer/src/seed.json");
    std::fs::write(&out_path, &json).expect("failed to write seed.json");

    let size_kb = json.len() / 1024;
    eprintln!("Wrote {out_path:?} ({size_kb} KB)");
    eprintln!("  pg_namespace: {}", snapshot.pg_namespace.len());
    eprintln!("  pg_type:      {}", snapshot.pg_type.len());
    eprintln!("  pg_enum:      {}", snapshot.pg_enum.len());
    eprintln!("  pg_range:     {}", snapshot.pg_range.len());
    eprintln!("  pg_class:     {}", snapshot.pg_class.len());
    eprintln!("  pg_attribute: {}", snapshot.pg_attribute.len());
    eprintln!("  pg_proc:      {}", snapshot.pg_proc.len());
    eprintln!("  pg_aggregate: {}", snapshot.pg_aggregate.len());
    eprintln!("  pg_operator:  {}", snapshot.pg_operator.len());
    eprintln!("  pg_cast:      {}", snapshot.pg_cast.len());
    eprintln!("  pg_extension: {}", snapshot.pg_extension.len());
    eprintln!("  pg_depend:    {}", snapshot.pg_depend.len());
    eprintln!("  pg_inherits:  {}", snapshot.pg_inherits.len());
    eprintln!("  pg_constraint:{}", snapshot.pg_constraint.len());
    eprintln!("  pg_index:     {}", snapshot.pg_index.len());
    eprintln!("  pg_rewrite:   {}", snapshot.pg_rewrite.len());
}

// ─── Schema export ─────────────────────────────────────────────────────────────

fn export_catalog(client: &mut postgres::Client) -> Result<PgCatalogSeed, postgres::Error> {
    let pg_namespace = export_namespaces(client)?;
    let nsname_by_oid: HashMap<PgNamespaceOid, String> = pg_namespace
        .iter()
        .map(|n| (n.oid, n.nspname.clone()))
        .collect();

    let pg_type = export_types(client)?;
    let pg_enum = export_enums(client)?;
    let pg_range = export_ranges(client)?;
    let pg_class = export_classes(client)?;
    let pg_attribute = export_attributes(client)?;
    let pg_proc = export_procs(client)?;
    let pg_aggregate = export_aggregates(client)?;
    let pg_operator = export_operators(client)?;
    let pg_cast = export_casts(client)?;
    let pg_extension = export_extensions(client)?;
    let pg_depend = export_depends(client)?;
    let pg_inherits = export_inherits(client)?;
    let pg_constraint = export_constraints(client)?;
    let pg_collation = export_collations(client)?;
    let search_path = export_search_path(client, &pg_namespace)?;
    let sql_function_defs = export_sql_function_defs(client)?;
    let (pg_am, pg_opfamily, pg_opclass) = export_access_methods(client)?;
    let pg_settings = export_settings(client)?;
    let pg_ts_objects = export_ts_objects(client)?;

    let _ = nsname_by_oid;

    // Build a seed without pg_index first: we need a working PgCatalog so
    // analyzer::serialize_expression / serialize_predicate can resolve OIDs
    // for the AstBindings of each indexed expression / WHERE predicate.
    let mut seed = PgCatalogSeed {
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
        pg_index: Vec::new(),
        // pg_rewrite (view bodies) is populated downstream by
        // populate_view_defs, which re-runs each CREATE VIEW through the
        // analyzer DDL pipeline against this seed.
        pg_rewrite: Vec::new(),
        pg_collation,
        search_path,
        sql_function_defs,
        pg_am,
        pg_opfamily,
        pg_opclass,
        pg_settings,
        pg_ts_objects,
    };
    let scratch = PgCatalog::from_seed(seed.clone());
    seed.pg_index = export_indexes(client, &scratch)?;
    Ok(seed)
}

fn export_search_path(
    client: &mut postgres::Client,
    namespaces: &[PgNamespace],
) -> Result<Vec<PgNamespaceOid>, postgres::Error> {
    let row = client.query_one("SHOW search_path", &[])?;
    let raw: String = row.get(0);
    let by_name: HashMap<&str, PgNamespaceOid> = namespaces
        .iter()
        .map(|n| (n.nspname.as_str(), n.oid))
        .collect();
    let mut oids: Vec<PgNamespaceOid> = Vec::new();
    let mut seen: std::collections::HashSet<PgNamespaceOid> = std::collections::HashSet::new();
    for part in raw.split(',') {
        let part = part.trim().trim_matches('"');
        let name = if part == "$user" || part == "\"$user\"" {
            "public"
        } else {
            part
        };
        if let Some(&oid) = by_name.get(name)
            && seen.insert(oid)
        {
            oids.push(oid);
        }
    }
    Ok(oids)
}

fn export_namespaces(client: &mut postgres::Client) -> Result<Vec<PgNamespace>, postgres::Error> {
    let rows = client.query(
        "SELECT oid, nspname FROM pg_catalog.pg_namespace ORDER BY oid",
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|r| {
            let oid: u32 = r.get(0);
            PgNamespace {
                oid: PgNamespaceOid::new(oid).expect("namespace oid is non-zero"),
                nspname: r.get(1),
            }
        })
        .collect())
}

fn export_types(client: &mut postgres::Client) -> Result<Vec<PgType>, postgres::Error> {
    let rows = client.query(
        "SELECT oid, typname, typnamespace, typtype, typcategory, typispreferred, \
                typrelid, typelem, typarray, typbasetype, typnotnull, typtypmod, typcollation, \
                typstorage \
         FROM pg_catalog.pg_type ORDER BY oid",
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|r| {
            let typtype: i8 = r.get(3);
            let typcategory: i8 = r.get(4);
            let oid: u32 = r.get(0);
            let typnamespace: u32 = r.get(2);
            let typrelid: u32 = r.get(6);
            let typelem: u32 = r.get(7);
            let typarray: u32 = r.get(8);
            let typbasetype: u32 = r.get(9);
            let typtypmod_raw: i32 = r.get(11);
            let typcollation: u32 = r.get(12);
            PgType {
                oid: PgTypeOid::new(oid).expect("pg_type.oid is non-zero"),
                typname: r.get(1),
                typnamespace: PgNamespaceOid::new(typnamespace).expect("typnamespace is non-zero"),
                typtype: char_to_typtype(typtype as u8 as char),
                typcategory: char_to_typcategory(typcategory as u8 as char),
                typispreferred: r.get(5),
                typrelid: PgClassOid::new(typrelid),
                typelem: PgTypeOid::new(typelem),
                typarray: PgTypeOid::new(typarray),
                typbasetype: PgTypeOid::new(typbasetype),
                typnotnull: r.get(10),
                typtypmod: (typtypmod_raw >= 0).then_some(typtypmod_raw),
                typcollation: PgCollationOid::new(typcollation),
                typstorage: match r.get::<_, i8>(13) as u8 {
                    b'e' => TypStorage::External,
                    b'm' => TypStorage::Main,
                    b'x' => TypStorage::Extended,
                    _ => TypStorage::Plain,
                },
            }
        })
        .collect())
}

fn export_enums(client: &mut postgres::Client) -> Result<Vec<PgEnum>, postgres::Error> {
    let rows = client.query(
        "SELECT oid, enumtypid, enumsortorder, enumlabel \
         FROM pg_catalog.pg_enum ORDER BY enumtypid, enumsortorder",
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|r| {
            let oid: u32 = r.get(0);
            let enumtypid: u32 = r.get(1);
            PgEnum {
                oid: PgEnumOid::new(oid).expect("pg_enum.oid is non-zero"),
                enumtypid: PgTypeOid::new(enumtypid).expect("enumtypid is non-zero"),
                enumsortorder: r.get(2),
                enumlabel: r.get(3),
            }
        })
        .collect())
}

fn export_ranges(client: &mut postgres::Client) -> Result<Vec<PgRange>, postgres::Error> {
    let rows = client.query(
        "SELECT rngtypid, rngsubtype, rngmultitypid FROM pg_catalog.pg_range ORDER BY rngtypid",
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|r| {
            let rngtypid: u32 = r.get(0);
            let rngsubtype: u32 = r.get(1);
            let rngmultitypid: u32 = r.get(2);
            PgRange {
                rngtypid: PgTypeOid::new(rngtypid).expect("rngtypid is non-zero"),
                rngsubtype: PgTypeOid::new(rngsubtype).expect("rngsubtype is non-zero"),
                rngmultitypid: PgTypeOid::new(rngmultitypid),
            }
        })
        .collect())
}

fn export_classes(client: &mut postgres::Client) -> Result<Vec<PgClass>, postgres::Error> {
    // No relkind filter — `RelKind` covers every variant PG emits, so the
    // mirror is 1:1. Rows we don't recognize are silently dropped by
    // `char_to_relkind`, but in practice that never fires.
    let rows = client.query(
        "SELECT oid, relname, relnamespace, relkind, reltype \
         FROM pg_catalog.pg_class \
         ORDER BY oid",
        &[],
    )?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let relkind: i8 = r.get(3);
            let relkind = char_to_relkind(relkind as u8 as char)?;
            let oid: u32 = r.get(0);
            let relnamespace: u32 = r.get(2);
            let reltype: u32 = r.get(4);
            Some(PgClass {
                oid: PgClassOid::new(oid).expect("pg_class.oid is non-zero"),
                relname: r.get(1),
                relnamespace: PgNamespaceOid::new(relnamespace).expect("relnamespace is non-zero"),
                relkind,
                reltype: PgTypeOid::new(reltype),
            })
        })
        .collect())
}

fn export_attributes(client: &mut postgres::Client) -> Result<Vec<PgAttribute>, postgres::Error> {
    // `attnum > 0` filters PG's system columns (`ctid`, `xmin`, …) which the
    // analyzer never inspects. `NOT attisdropped` skips columns that
    // ALTER TABLE DROP COLUMN tombstoned — PG keeps the row so attnum gaps
    // stay stable. No relkind filter: we mirror every relation's columns.
    let rows = client.query(
        "SELECT a.attrelid, a.attname, a.atttypid, a.attnum, a.attnotnull, a.atthasdef, \
                a.attgenerated, a.atttypmod, a.attidentity, a.attcollation, \
                a.attislocal, a.attinhcount::int2 \
         FROM pg_catalog.pg_attribute a \
         WHERE a.attnum > 0 \
           AND NOT a.attisdropped \
         ORDER BY a.attrelid, a.attnum",
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|r| {
            let attgenerated: i8 = r.get(6);
            let attidentity: i8 = r.get(8);
            let attrelid: u32 = r.get(0);
            let atttypid: u32 = r.get(2);
            let atttypmod_raw: i32 = r.get(7);
            let attcollation: u32 = r.get(9);
            PgAttribute {
                attrelid: PgClassOid::new(attrelid).expect("attrelid is non-zero"),
                attname: r.get(1),
                atttypid: PgTypeOid::new(atttypid).expect("atttypid is non-zero"),
                attnum: r.get(3),
                attnotnull: r.get(4),
                atthasdef: r.get(5),
                attgenerated: char_to_attgenerated(attgenerated as u8 as char),
                atttypmod: (atttypmod_raw >= 0).then_some(atttypmod_raw),
                attidentity: char_to_attidentity(attidentity as u8 as char),
                attcollation: PgCollationOid::new(attcollation),
                attislocal: r.get(10),
                attinhcount: r.get(11),
            }
        })
        .collect())
}

fn export_procs(client: &mut postgres::Client) -> Result<Vec<PgProc>, postgres::Error> {
    let rows = client.query(
        "SELECT p.oid, p.proname, p.pronamespace, p.prokind, \
                p.proargtypes::int4[]::int4[], p.prorettype, \
                p.proretset, p.provariadic, p.proisstrict, p.pronargdefaults, \
                p.proallargtypes::int4[], p.proargmodes, p.proargnames, \
                p.provolatile, \
                (SELECT coalesce(array_agg(t.typtype = 'p' AND t.typname LIKE 'any%' \
                                           ORDER BY u.ord), '{}') \
                   FROM unnest(p.proargtypes::oid[]) WITH ORDINALITY u(o, ord) \
                   JOIN pg_catalog.pg_type t ON t.oid = u.o) \
         FROM pg_catalog.pg_proc p \
         ORDER BY p.oid",
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|r| {
            let prokind: i8 = r.get(3);
            let arg_types_raw: Vec<i32> = r.get(4);
            let proargtypes: Vec<PgTypeOid> = arg_types_raw
                .iter()
                .filter_map(|&x| PgTypeOid::new(x as u32))
                .collect();
            let proallargtypes_raw: Option<Vec<i32>> = r.get(10);
            let proallargtypes: Vec<PgTypeOid> = proallargtypes_raw
                .map(|v| {
                    v.into_iter()
                        .filter_map(|x| PgTypeOid::new(x as u32))
                        .collect()
                })
                .unwrap_or_default();
            let arg_modes: Option<Vec<i8>> = r.get(11);
            let proargmodes: Vec<ArgMode> = arg_modes
                .map(|v| {
                    v.into_iter()
                        .map(|c| char_to_argmode(c as u8 as char))
                        .collect()
                })
                .unwrap_or_default();
            let arg_names: Option<Vec<String>> = r.get(12);
            let provolatile: i8 = r.get(13);
            let oid: u32 = r.get(0);
            let pronamespace: u32 = r.get(2);
            let prorettype: u32 = r.get(5);
            let provariadic: u32 = r.get(7);
            let pronargdefaults: i16 = r.get(9);
            let polymorphic_args: Vec<bool> = r.get(14);
            // The builtin defaults' types: a default of a non-polymorphic
            // parameter was coerced to the parameter's type when the
            // function was created, so its type is the declared one. A
            // polymorphic parameter's default keeps its own type, which the
            // catalog doesn't expose — refuse to guess.
            let first_default = proargtypes.len().saturating_sub(pronargdefaults as usize);
            if polymorphic_args.iter().skip(first_default).any(|&p| p) {
                panic!(
                    "pg_proc {oid} ({}) has a default on a polymorphic parameter; \
                     typedpg_seed can't export its type",
                    r.get::<_, String>(1)
                );
            }
            let proargdefaulttypes = proargtypes[first_default..].to_vec();
            PgProc {
                oid: PgProcOid::new(oid).expect("pg_proc.oid is non-zero"),
                proname: r.get(1),
                pronamespace: PgNamespaceOid::new(pronamespace).expect("pronamespace is non-zero"),
                prokind: char_to_prokind(prokind as u8 as char),
                proargtypes,
                prorettype: PgTypeOid::new(prorettype).expect("prorettype is non-zero"),
                proretset: r.get(6),
                provariadic: PgTypeOid::new(provariadic),
                proisstrict: r.get(8),
                pronargdefaults,
                proallargtypes,
                proargmodes,
                proargnames: arg_names.unwrap_or_default(),
                provolatile: char_to_provolatile(provolatile as u8 as char),
                proargdefaulttypes,
            }
        })
        .collect())
}

fn char_to_provolatile(c: char) -> ProVolatile {
    match c {
        'i' => ProVolatile::Immutable,
        's' => ProVolatile::Stable,
        _ => ProVolatile::Volatile,
    }
}

fn export_aggregates(client: &mut postgres::Client) -> Result<Vec<PgAggregate>, postgres::Error> {
    // aggfnoid identifies the aggregate (FK pg_proc.oid). aggfinalfn (also
    // FK pg_proc.oid, or 0 for none) drives the effective return type
    // resolution: callers walk to that proc's prorettype, otherwise fall
    // back to aggfnoid.prorettype. Don't pre-compute the type — PG doesn't
    // and we shouldn't desync from it.
    let rows = client.query(
        "SELECT aggfnoid::int4 AS aggfnoid, \
                aggfinalfn::int4 AS aggfinalfn, \
                aggkind::text AS aggkind, \
                aggnumdirectargs \
         FROM pg_catalog.pg_aggregate \
         ORDER BY aggfnoid",
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|r| {
            let aggfnoid: i32 = r.get(0);
            let aggfinalfn: i32 = r.get(1);
            let aggkind: String = r.get(2);
            PgAggregate {
                aggfnoid: PgProcOid::new(aggfnoid as u32).expect("aggfnoid is non-zero"),
                aggfinalfn: PgProcOid::new(aggfinalfn as u32),
                aggkind: match aggkind.as_str() {
                    "o" => AggKind::OrderedSet,
                    "h" => AggKind::Hypothetical,
                    _ => AggKind::Normal,
                },
                aggnumdirectargs: r.get(3),
            }
        })
        .collect())
}

fn export_operators(client: &mut postgres::Client) -> Result<Vec<PgOperator>, postgres::Error> {
    // No filter — shell operators (oprresult = 0) round-trip too. Operator
    // resolution skips them at lookup time.
    let rows = client.query(
        "SELECT oid, oprname, oprnamespace, oprleft, oprright, oprresult, oprcode::oid \
         FROM pg_catalog.pg_operator \
         ORDER BY oid",
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|r| {
            let oid: u32 = r.get(0);
            let oprnamespace: u32 = r.get(2);
            let oprleft: u32 = r.get(3);
            let oprright: u32 = r.get(4);
            let oprresult: u32 = r.get(5);
            let oprcode: u32 = r.get(6);
            PgOperator {
                oid: PgOperatorOid::new(oid).expect("pg_operator.oid is non-zero"),
                oprname: r.get(1),
                oprnamespace: PgNamespaceOid::new(oprnamespace).expect("oprnamespace is non-zero"),
                oprleft: PgTypeOid::new(oprleft),
                oprright: PgTypeOid::new(oprright).expect("oprright is non-zero"),
                oprresult: PgTypeOid::new(oprresult),
                oprcode: PgProcOid::new(oprcode),
            }
        })
        .collect())
}

fn export_casts(client: &mut postgres::Client) -> Result<Vec<PgCast>, postgres::Error> {
    let rows = client.query(
        "SELECT oid, castsource, casttarget, castcontext, castmethod, castfunc \
         FROM pg_catalog.pg_cast \
         ORDER BY oid",
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|r| {
            let ctx: i8 = r.get(3);
            let method: i8 = r.get(4);
            let oid: u32 = r.get(0);
            let castsource: u32 = r.get(1);
            let casttarget: u32 = r.get(2);
            let castfunc: u32 = r.get(5);
            PgCast {
                oid: PgCastOid::new(oid).expect("pg_cast.oid is non-zero"),
                castsource: PgTypeOid::new(castsource).expect("castsource is non-zero"),
                casttarget: PgTypeOid::new(casttarget).expect("casttarget is non-zero"),
                castcontext: char_to_castcontext(ctx as u8 as char),
                castmethod: char_to_castmethod(method as u8 as char),
                castfunc: PgProcOid::new(castfunc),
            }
        })
        .collect())
}

fn export_extensions(client: &mut postgres::Client) -> Result<Vec<PgExtension>, postgres::Error> {
    let rows = client.query(
        "SELECT oid, extname, extnamespace, extversion \
         FROM pg_catalog.pg_extension \
         ORDER BY oid",
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|r| {
            let oid: u32 = r.get(0);
            let extnamespace: u32 = r.get(2);
            PgExtension {
                oid: PgExtensionOid::new(oid).expect("pg_extension.oid is non-zero"),
                extname: r.get(1),
                extnamespace: PgNamespaceOid::new(extnamespace).expect("extnamespace is non-zero"),
                extversion: r.get(3),
            }
        })
        .collect())
}

fn export_constraints(client: &mut postgres::Client) -> Result<Vec<PgConstraint>, postgres::Error> {
    // No contype filter — `ConType::Other` is a catch-all so unknown chars
    // round-trip without panicking.
    let rows = client.query(
        "SELECT oid, conname, conrelid, contype, conkey, confrelid, confkey, \
                conislocal, coninhcount::int2 \
         FROM pg_catalog.pg_constraint \
         ORDER BY oid",
        &[],
    )?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let oid: u32 = r.get(0);
            let conname: String = r.get(1);
            let conrelid: u32 = r.get(2);
            let contype: i8 = r.get(3);
            let conkey: Option<Vec<i16>> = r.get(4);
            let confrelid: u32 = r.get(5);
            let confkey: Option<Vec<i16>> = r.get(6);
            Some(PgConstraint {
                oid: PgConstraintOid::new(oid)?,
                conname,
                conrelid: PgClassOid::new(conrelid)?,
                contype: char_to_contype(contype as u8 as char),
                conkey: conkey.unwrap_or_default(),
                confrelid: PgClassOid::new(confrelid),
                confkey: confkey.unwrap_or_default(),
                conislocal: r.get(7),
                coninhcount: r.get(8),
            })
        })
        .collect())
}

fn char_to_contype(c: char) -> ConType {
    match c {
        'p' => ConType::PrimaryKey,
        'u' => ConType::Unique,
        'f' => ConType::ForeignKey,
        'c' => ConType::Check,
        'x' => ConType::Exclusion,
        'n' => ConType::NotNull,
        _ => ConType::Other,
    }
}

/// Export every `pg_index` row whose `indrelid` is a relation we already
/// snapshotted. For each row we also need to recover `indexprs` and
/// `indpred` as analyzer ASTs — PG stores them as `pg_node_tree` (its own
/// serialization, not the protobuf shape we use), so we reach back through
/// `pg_get_indexdef(idx, slot, false)` and `pg_get_expr(indpred, indrelid)`
/// to get the SQL text and feed it to the analyzer's `serialize_expression`
/// / `serialize_predicate`. Indexes whose expressions don't round-trip
/// (rare; usually catalog-internal collation/typename quirks) are skipped
/// with a warning.
fn export_indexes(
    client: &mut postgres::Client,
    snapshot: &PgCatalog,
) -> Result<Vec<PgIndex>, postgres::Error> {
    let rows = client.query(
        "SELECT i.indexrelid, i.indrelid, i.indnatts, i.indnkeyatts, \
                i.indisunique, i.indisprimary, \
                i.indkey::int2[]::int4[] AS indkey, \
                i.indexprs IS NOT NULL AS has_exprs, \
                CASE WHEN i.indpred IS NOT NULL \
                     THEN pg_get_expr(i.indpred, i.indrelid) \
                     ELSE NULL END AS pred_sql \
         FROM pg_catalog.pg_index i \
         ORDER BY i.indexrelid",
        &[],
    )?;
    let mut out = Vec::with_capacity(rows.len());
    let mut skipped: Vec<u32> = Vec::new();
    for r in rows.iter() {
        let indexrelid: u32 = r.get(0);
        let indrelid: u32 = r.get(1);
        let indnatts: i16 = r.get(2);
        let indnkeyatts: i16 = r.get(3);
        let indisunique: bool = r.get(4);
        let indisprimary: bool = r.get(5);
        let indkey_raw: Vec<i32> = r.get(6);
        let has_exprs: bool = r.get(7);
        let pred_sql: Option<String> = r.get(8);
        let indkey: Vec<i16> = indkey_raw.into_iter().map(|n| n as i16).collect();

        // Pull each expression slot's SQL via pg_get_indexdef(idx, slot, false).
        // PG numbers slots from 1 up; slots whose indkey is 0 are expressions.
        let mut indexprs = Vec::new();
        let mut skip_row = false;
        if has_exprs {
            for (slot_idx, &attnum) in indkey.iter().enumerate() {
                if attnum != 0 {
                    continue;
                }
                let slot_no = (slot_idx + 1) as i32;
                let expr_sql: String = client
                    .query_one(
                        "SELECT pg_get_indexdef($1::oid, $2, false)",
                        &[&indexrelid, &slot_no],
                    )?
                    .get(0);
                match snapshot.serialize_expression(&expr_sql) {
                    Ok(s) => indexprs.push(s),
                    Err(e) => {
                        eprintln!(
                            "  skipping pg_index oid={indexrelid}: serialize_expression on `{expr_sql}` failed: {e}"
                        );
                        skip_row = true;
                        break;
                    }
                }
            }
        }
        if skip_row {
            skipped.push(indexrelid);
            continue;
        }

        let indpred = if let Some(sql) = pred_sql {
            match snapshot.serialize_predicate(&sql) {
                Ok(s) => Some(s),
                Err(e) => {
                    eprintln!(
                        "  skipping pg_index oid={indexrelid}: serialize_predicate on `{sql}` failed: {e}"
                    );
                    skipped.push(indexrelid);
                    continue;
                }
            }
        } else {
            None
        };

        let Some(indexrelid) = PgClassOid::new(indexrelid) else {
            continue;
        };
        let Some(indrelid) = PgClassOid::new(indrelid) else {
            continue;
        };
        out.push(PgIndex {
            indexrelid,
            indrelid,
            indnatts,
            indnkeyatts,
            indisunique,
            indisprimary,
            indkey,
            indexprs,
            indpred,
        });
    }
    if !skipped.is_empty() {
        eprintln!(
            "Skipped {} pg_index row(s) whose expressions/predicate didn't round-trip.",
            skipped.len(),
        );
    }
    Ok(out)
}

fn export_collations(client: &mut postgres::Client) -> Result<Vec<PgCollation>, postgres::Error> {
    // Collations vary by host OS — `en_US.UTF-8` may exist on one machine
    // and not another. The seed snapshots whatever the container shipped
    // with; users that build against a different libc will need to
    // regenerate.
    let rows = client.query(
        "SELECT oid, collname, collnamespace, collencoding \
         FROM pg_catalog.pg_collation \
         ORDER BY oid",
        &[],
    )?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let oid: u32 = r.get(0);
            let collnamespace: u32 = r.get(2);
            Some(PgCollation {
                oid: PgCollationOid::new(oid)?,
                collname: r.get(1),
                collnamespace: PgNamespaceOid::new(collnamespace)?,
                collencoding: r.get(3),
            })
        })
        .collect())
}

fn export_inherits(client: &mut postgres::Client) -> Result<Vec<PgInherits>, postgres::Error> {
    let rows = client.query(
        "SELECT inhrelid, inhparent, inhseqno \
         FROM pg_catalog.pg_inherits \
         ORDER BY inhrelid, inhseqno",
        &[],
    )?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let inhrelid: u32 = r.get(0);
            let inhparent: u32 = r.get(1);
            Some(PgInherits {
                inhrelid: PgClassOid::new(inhrelid)?,
                inhparent: PgClassOid::new(inhparent)?,
                inhseqno: r.get(2),
            })
        })
        .collect())
}

fn export_depends(client: &mut postgres::Client) -> Result<Vec<PgDepend>, postgres::Error> {
    // No deptype filter — every dependency edge round-trips. Mirrors
    // `pg_depend` 1:1 so cascading drops behave identically to PG.
    let rows = client.query(
        "SELECT classid::int4, objid::int4, objsubid, refclassid::int4, refobjid::int4, \
                refobjsubid, deptype \
         FROM pg_catalog.pg_depend \
         ORDER BY classid, objid, objsubid, refclassid, refobjid",
        &[],
    )?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let classid: i32 = r.get(0);
            let objid: i32 = r.get(1);
            let objsubid: i32 = r.get(2);
            let refclassid: i32 = r.get(3);
            let refobjid: i32 = r.get(4);
            let refobjsubid: i32 = r.get(5);
            let deptype: i8 = r.get(6);
            Some(PgDepend {
                classid: PgClassOid::new(classid as u32)?,
                objid: PgGenericOid::new(objid as u32)?,
                objsubid: objsubid as i16,
                refclassid: PgClassOid::new(refclassid as u32)?,
                refobjid: PgGenericOid::new(refobjid as u32)?,
                refobjsubid: refobjsubid as i16,
                deptype: char_to_deptype(deptype as u8 as char),
            })
        })
        .collect())
}

// ─── View definitions (second pass) ────────────────────────────────────────────

/// Text search configurations, dictionaries, parsers and templates.
fn export_ts_objects(client: &mut postgres::Client) -> Result<Vec<PgTsObject>, postgres::Error> {
    Ok(client
        .query(
            "SELECT 'c', cfgname::text, cfgnamespace FROM pg_catalog.pg_ts_config \
             UNION ALL SELECT 'd', dictname::text, dictnamespace FROM pg_catalog.pg_ts_dict \
             UNION ALL SELECT 'p', prsname::text, prsnamespace FROM pg_catalog.pg_ts_parser \
             UNION ALL SELECT 't', tmplname::text, tmplnamespace FROM pg_catalog.pg_ts_template \
             ORDER BY 1, 2",
            &[],
        )?
        .iter()
        .filter_map(|r| {
            Some(PgTsObject {
                kind: r.get(0),
                name: r.get(1),
                namespace: PgNamespaceOid::new(r.get::<_, u32>(2))?,
            })
        })
        .collect())
}

/// The configuration parameters (`pg_settings`) of a stock server.
fn export_settings(client: &mut postgres::Client) -> Result<Vec<PgSetting>, postgres::Error> {
    Ok(client
        .query(
            "SELECT name, vartype, context, coalesce(unit, ''), coalesce(min_val, ''), \
                    coalesce(max_val, ''), coalesce(enumvals, '{}') \
             FROM pg_catalog.pg_settings ORDER BY name",
            &[],
        )?
        .iter()
        .map(|r| PgSetting {
            name: r.get(0),
            vartype: r.get(1),
            context: r.get(2),
            unit: r.get(3),
            min_val: r.get(4),
            max_val: r.get(5),
            enumvals: r.get(6),
        })
        .collect())
}

/// `pg_am`, `pg_opfamily` and `pg_opclass` rows.
type AccessMethodRows = (Vec<PgAm>, Vec<PgOpfamily>, Vec<PgOpclass>);

/// Access methods, operator families and operator classes (by access
/// method name).
fn export_access_methods(
    client: &mut postgres::Client,
) -> Result<AccessMethodRows, postgres::Error> {
    let am = client
        .query(
            "SELECT amname::text, amtype::text FROM pg_catalog.pg_am ORDER BY oid",
            &[],
        )?
        .iter()
        .map(|r| PgAm {
            amname: r.get(0),
            amtype: r.get(1),
        })
        .collect();
    let opfamily = client
        .query(
            "SELECT f.opfname::text, f.opfnamespace, a.amname::text \
             FROM pg_catalog.pg_opfamily f JOIN pg_catalog.pg_am a ON a.oid = f.opfmethod \
             ORDER BY f.oid",
            &[],
        )?
        .iter()
        .filter_map(|r| {
            Some(PgOpfamily {
                opfname: r.get(0),
                opfnamespace: PgNamespaceOid::new(r.get::<_, u32>(1))?,
                opfmethod: r.get(2),
            })
        })
        .collect();
    let opclass = client
        .query(
            "SELECT c.opcname::text, c.opcnamespace, a.amname::text, c.opcintype, c.opcdefault \
             FROM pg_catalog.pg_opclass c JOIN pg_catalog.pg_am a ON a.oid = c.opcmethod \
             ORDER BY c.oid",
            &[],
        )?
        .iter()
        .filter_map(|r| {
            Some(PgOpclass {
                opcname: r.get(0),
                opcnamespace: PgNamespaceOid::new(r.get::<_, u32>(1))?,
                opcmethod: r.get(2),
                opcintype: PgTypeOid::new(r.get::<_, u32>(3))?,
                opcdefault: r.get(4),
            })
        })
        .collect();
    Ok((am, opfamily, opclass))
}

/// `pg_get_functiondef` of each non-set-returning `LANGUAGE sql` function;
/// the analyzer keeps the inlinable ones' bodies (`inline_function`).
fn export_sql_function_defs(
    client: &mut postgres::Client,
) -> Result<Vec<(PgProcOid, String)>, postgres::Error> {
    let rows = client.query(
        "SELECT p.oid, pg_catalog.pg_get_functiondef(p.oid) \
         FROM pg_catalog.pg_proc p \
         WHERE p.prolang = (SELECT oid FROM pg_catalog.pg_language WHERE lanname = 'sql') \
           AND p.prokind = 'f' AND NOT p.proretset \
         ORDER BY p.oid",
        &[],
    )?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let oid = PgProcOid::new(r.get::<_, u32>(0))?;
            let definition: String = r.get(1);
            Some((oid, definition))
        })
        .collect())
}

fn export_view_definitions(
    client: &mut postgres::Client,
) -> Result<Vec<(String, String, String)>, postgres::Error> {
    let rows = client.query(
        "SELECT schemaname, viewname, definition FROM pg_catalog.pg_views \
         UNION ALL \
         SELECT schemaname, matviewname, definition FROM pg_catalog.pg_matviews \
         ORDER BY 1, 2",
        &[],
    )?;
    Ok(rows
        .iter()
        .map(|r| {
            let schema: String = r.get(0);
            let name: String = r.get(1);
            let definition: String = r.get(2);
            (schema, name, definition)
        })
        .collect())
}

/// Re-apply each view's `CREATE VIEW` through the analyzer's DDL pipeline so
/// the matching `pg_rewrite._RETURN` row gets emitted. Continues to use
/// `to_seed()`/`from_seed()` for round-trip.
///
/// Views the analyzer can't yet handle (typically polymorphic /
/// information_schema oddities) are logged and skipped — pg_rewrite stays
/// empty for those, but the rest of the seed is still produced.
fn populate_view_defs(seed: PgCatalogSeed, defs: Vec<(String, String, String)>) -> PgCatalogSeed {
    let mut db = PgCatalog::from_seed(seed);
    let mut failed = Vec::new();

    for (schema, name, definition) in &defs {
        let qn = QualifiedName::new(schema.clone(), name.clone());

        let before: Vec<(String, PgTypeOid)> = db
            .resolve_table(Some(schema), name)
            .map(|c| {
                db.attributes_of(c.oid)
                    .iter()
                    .map(|a| (a.attname.clone(), a.atttypid))
                    .collect()
            })
            .unwrap_or_default();

        let sql = format!(
            "CREATE OR REPLACE VIEW {qn} AS {body}",
            qn = qn,
            body = definition.trim_end().trim_end_matches(';'),
        );

        if let Err(err) = db.apply_sql(&sql) {
            log_view_failure(&qn, &err.to_string(), &sql);
            failed.push(qn.to_string());
            continue;
        }

        let after: Vec<(String, PgTypeOid)> = db
            .resolve_table(Some(schema), name)
            .map(|c| {
                db.attributes_of(c.oid)
                    .iter()
                    .map(|a| (a.attname.clone(), a.atttypid))
                    .collect()
            })
            .unwrap_or_default();

        if let Some(drift) = describe_column_drift(&before, &after) {
            log_view_failure(&qn, &format!("column drift: {drift}"), &sql);
            failed.push(qn.to_string());
        }
    }

    if !failed.is_empty() {
        eprintln!(
            "\n{} view(s) failed to reanalyze (see errors above):",
            failed.len()
        );
        for s in &failed {
            eprintln!("  - {s}");
        }
        panic!(
            "refusing to emit incomplete seed.json: {} view(s) did not round-trip through the analyzer",
            failed.len()
        );
    }

    db.to_seed()
}

fn log_view_failure(qn: &QualifiedName, error: &str, sql: &str) {
    eprintln!("--- {qn} ---");
    eprintln!("  error: {error}");
    eprintln!("  sql: {sql}");
}

fn describe_column_drift(
    before: &[(String, PgTypeOid)],
    after: &[(String, PgTypeOid)],
) -> Option<String> {
    if before.len() != after.len() {
        return Some(format!("column count {} → {}", before.len(), after.len()));
    }
    for (i, (b, a)) in before.iter().zip(after.iter()).enumerate() {
        if b.0 != a.0 {
            return Some(format!("column #{i} name {:?} → {:?}", b.0, a.0));
        }
        if b.1 != a.1 {
            return Some(format!(
                "column #{i} type_oid {} → {}",
                b.1.get(),
                a.1.get()
            ));
        }
    }
    None
}

// ─── Char → enum mapping ───────────────────────────────────────────────────────

fn char_to_typtype(c: char) -> TypType {
    match c {
        'b' => TypType::Base,
        'c' => TypType::Composite,
        'd' => TypType::Domain,
        'e' => TypType::Enum,
        'p' => TypType::Pseudo,
        'r' => TypType::Range,
        'm' => TypType::Multirange,
        _ => TypType::Base,
    }
}

fn char_to_typcategory(c: char) -> TypCategory {
    match c {
        'A' => TypCategory::Array,
        'B' => TypCategory::Boolean,
        'C' => TypCategory::Composite,
        'D' => TypCategory::DateTime,
        'E' => TypCategory::Enum,
        'G' => TypCategory::Geometric,
        'I' => TypCategory::Network,
        'N' => TypCategory::Numeric,
        'P' => TypCategory::Pseudo,
        'R' => TypCategory::Range,
        'S' => TypCategory::String,
        'T' => TypCategory::Timespan,
        'U' => TypCategory::UserDefined,
        'V' => TypCategory::BitString,
        'X' => TypCategory::Unknown,
        'Z' => TypCategory::Internal,
        _ => TypCategory::UserDefined,
    }
}

fn char_to_relkind(c: char) -> Option<RelKind> {
    Some(match c {
        'r' => RelKind::Table,
        'i' => RelKind::Index,
        'S' => RelKind::Sequence,
        't' => RelKind::ToastTable,
        'v' => RelKind::View,
        'm' => RelKind::MaterializedView,
        'c' => RelKind::CompositeType,
        'f' => RelKind::ForeignTable,
        'p' => RelKind::Partitioned,
        'I' => RelKind::PartitionedIndex,
        _ => return None,
    })
}

fn char_to_prokind(c: char) -> ProKind {
    match c {
        'a' => ProKind::Aggregate,
        'w' => ProKind::Window,
        'p' => ProKind::Procedure,
        _ => ProKind::Function,
    }
}

fn char_to_argmode(c: char) -> ArgMode {
    match c {
        'o' => ArgMode::Out,
        'b' => ArgMode::InOut,
        'v' => ArgMode::Variadic,
        't' => ArgMode::Table,
        _ => ArgMode::In,
    }
}

fn char_to_attgenerated(c: char) -> Option<AttGenerated> {
    match c {
        's' => Some(AttGenerated::Stored),
        'v' => Some(AttGenerated::Virtual),
        _ => None,
    }
}

fn char_to_attidentity(c: char) -> Option<AttIdentity> {
    match c {
        'a' => Some(AttIdentity::Always),
        'd' => Some(AttIdentity::ByDefault),
        _ => None,
    }
}

fn char_to_castcontext(c: char) -> CastContext {
    match c {
        'i' => CastContext::Implicit,
        'a' => CastContext::Assignment,
        _ => CastContext::Explicit,
    }
}

fn char_to_castmethod(c: char) -> CastMethod {
    match c {
        'b' => CastMethod::Binary,
        'i' => CastMethod::InOut,
        _ => CastMethod::Function,
    }
}

fn char_to_deptype(c: char) -> DepType {
    match c {
        'a' => DepType::Auto,
        'i' => DepType::Internal,
        'e' => DepType::Extension,
        'x' => DepType::AutoExtension,
        'p' => DepType::Pin,
        _ => DepType::Normal,
    }
}
