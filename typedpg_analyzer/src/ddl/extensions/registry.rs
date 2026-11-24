//! The extension registry: every supported extension, its default
//! version, and the SQL script chain (base install + upgrades).
//!
//! This is pure data — the install/upgrade logic lives in the parent
//! `extensions` module. Child modules can name the parent's private
//! `ExtensionDef`/`ExtensionVersion` structs, so they stay defined there.

use super::{ExtensionDef, ExtensionVersion, Script};

/// A bundled script, `src/extensions/<file>`, embedded as its zstd
/// compression `<file>.zst` (written by update_extensions.sh).
macro_rules! script {
    ($file:literal) => {
        Script::Zstd(include_bytes!(concat!("../../extensions/", $file, ".zst")))
    };
}

// ─── Registry ───────────────────────────────────────────────────────────────
//
// All PostgreSQL contrib extensions, alphabetical order, then the
// third-party ones (PostGIS, pg_cron, pgmq, pg_jsonschema, pgvector).
// SQL files are in typedpg_analyzer/src/extensions/ (embedded compressed,
// `<file>.sql.zst`) and fetched via update_extensions.sh from the PostgreSQL
// release typedpg targets: the versions and default versions here are that
// release's, as its parser must accept every script and its server has
// every object they create.

// PostGIS reaches a release from an older one through the `ANY`
// pseudo-version (`postgis_extensions_upgrade()` updates to `ANY`, then to
// the release, even when already on it), and packages loose objects through
// `unpackaged`. Their scripts back to 3.7.0 (`--ANY--3.7.0`,
// `--unpackaged--3.7.0`, about 17 MB) are not bundled. The only `ANY` the
// registry reaches is tagged from 3.7.0, whose objects the full upgrade
// leaves as they are, so updating back needs no script (the extension
// oracle checks it against the server). Packaging loose objects is left out.

pub(super) static REGISTRY: &[ExtensionDef] = &[
    // ── amcheck ────────────────────────────────────────────────────────
    ExtensionDef {
        name: "amcheck",
        default_version: "1.5",
        versions: &[
            ExtensionVersion {
                version: "1.0",
                from: None,
                sql: script!("amcheck--1.0.sql"),
            },
            ExtensionVersion {
                version: "1.1",
                from: Some("1.0"),
                sql: script!("amcheck--1.0--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: script!("amcheck--1.1--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: script!("amcheck--1.2--1.3.sql"),
            },
            ExtensionVersion {
                version: "1.4",
                from: Some("1.3"),
                sql: script!("amcheck--1.3--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: script!("amcheck--1.4--1.5.sql"),
            },
        ],
    },
    // ── autoinc (SPI) ──────────────────────────────────────────────────
    ExtensionDef {
        name: "autoinc",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("autoinc--1.0.sql"),
        }],
    },
    // ── bloom ──────────────────────────────────────────────────────────
    ExtensionDef {
        name: "bloom",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("bloom--1.0.sql"),
        }],
    },
    // ── bool_plperl (requires plperl) ──────────────────────────────────
    ExtensionDef {
        name: "bool_plperl",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("bool_plperl--1.0.sql"),
        }],
    },
    // ── bool_plperlu (requires plperlu) ────────────────────────────────
    ExtensionDef {
        name: "bool_plperlu",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("bool_plperlu--1.0.sql"),
        }],
    },
    // ── btree_gin ──────────────────────────────────────────────────────
    ExtensionDef {
        name: "btree_gin",
        default_version: "1.4",
        versions: &[
            ExtensionVersion {
                version: "1.0",
                from: None,
                sql: script!("btree_gin--1.0.sql"),
            },
            ExtensionVersion {
                version: "1.1",
                from: Some("1.0"),
                sql: script!("btree_gin--1.0--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: script!("btree_gin--1.1--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: script!("btree_gin--1.2--1.3.sql"),
            },
            ExtensionVersion {
                version: "1.4",
                from: Some("1.3"),
                sql: script!("btree_gin--1.3--1.4.sql"),
            },
        ],
    },
    // ── btree_gist (PostgreSQL 19 installs 1.9 directly) ───────────────
    ExtensionDef {
        name: "btree_gist",
        default_version: "1.9",
        versions: &[ExtensionVersion {
            version: "1.9",
            from: None,
            sql: script!("btree_gist--1.9.sql"),
        }],
    },
    // ── citext ─────────────────────────────────────────────────────────
    ExtensionDef {
        name: "citext",
        default_version: "1.8",
        versions: &[
            ExtensionVersion {
                version: "1.4",
                from: None,
                sql: script!("citext--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: script!("citext--1.4--1.5.sql"),
            },
            ExtensionVersion {
                version: "1.6",
                from: Some("1.5"),
                sql: script!("citext--1.5--1.6.sql"),
            },
            ExtensionVersion {
                version: "1.7",
                from: Some("1.6"),
                sql: script!("citext--1.6--1.7.sql"),
            },
            ExtensionVersion {
                version: "1.8",
                from: Some("1.7"),
                sql: script!("citext--1.7--1.8.sql"),
            },
        ],
    },
    // ── cube ───────────────────────────────────────────────────────────
    ExtensionDef {
        name: "cube",
        default_version: "1.5",
        versions: &[
            ExtensionVersion {
                version: "1.2",
                from: None,
                sql: script!("cube--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: script!("cube--1.2--1.3.sql"),
            },
            ExtensionVersion {
                version: "1.4",
                from: Some("1.3"),
                sql: script!("cube--1.3--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: script!("cube--1.4--1.5.sql"),
            },
        ],
    },
    // ── dblink ─────────────────────────────────────────────────────────
    ExtensionDef {
        name: "dblink",
        default_version: "1.2",
        versions: &[ExtensionVersion {
            version: "1.2",
            from: None,
            sql: script!("dblink--1.2.sql"),
        }],
    },
    // ── dict_int ───────────────────────────────────────────────────────
    ExtensionDef {
        name: "dict_int",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("dict_int--1.0.sql"),
        }],
    },
    // ── dict_xsyn ──────────────────────────────────────────────────────
    ExtensionDef {
        name: "dict_xsyn",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("dict_xsyn--1.0.sql"),
        }],
    },
    // ── earthdistance (depends on cube) ────────────────────────────────
    ExtensionDef {
        name: "earthdistance",
        default_version: "1.2",
        versions: &[
            ExtensionVersion {
                version: "1.1",
                from: None,
                sql: script!("earthdistance--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: script!("earthdistance--1.1--1.2.sql"),
            },
        ],
    },
    // ── file_fdw ───────────────────────────────────────────────────────
    ExtensionDef {
        name: "file_fdw",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("file_fdw--1.0.sql"),
        }],
    },
    // ── fuzzystrmatch ──────────────────────────────────────────────────
    ExtensionDef {
        name: "fuzzystrmatch",
        default_version: "1.2",
        versions: &[
            ExtensionVersion {
                version: "1.1",
                from: None,
                sql: script!("fuzzystrmatch--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: script!("fuzzystrmatch--1.1--1.2.sql"),
            },
        ],
    },
    // ── hstore ─────────────────────────────────────────────────────────
    ExtensionDef {
        name: "hstore",
        default_version: "1.8",
        versions: &[
            ExtensionVersion {
                version: "1.4",
                from: None,
                sql: script!("hstore--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: script!("hstore--1.4--1.5.sql"),
            },
            ExtensionVersion {
                version: "1.6",
                from: Some("1.5"),
                sql: script!("hstore--1.5--1.6.sql"),
            },
            ExtensionVersion {
                version: "1.7",
                from: Some("1.6"),
                sql: script!("hstore--1.6--1.7.sql"),
            },
            ExtensionVersion {
                version: "1.8",
                from: Some("1.7"),
                sql: script!("hstore--1.7--1.8.sql"),
            },
        ],
    },
    // ── hstore_plperl (requires hstore + plperl) ───────────────────────
    ExtensionDef {
        name: "hstore_plperl",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("hstore_plperl--1.0.sql"),
        }],
    },
    // ── hstore_plperlu (requires hstore + plperlu) ─────────────────────
    ExtensionDef {
        name: "hstore_plperlu",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("hstore_plperlu--1.0.sql"),
        }],
    },
    // ── hstore_plpython3u (requires hstore + plpython3u) ───────────────
    ExtensionDef {
        name: "hstore_plpython3u",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("hstore_plpython3u--1.0.sql"),
        }],
    },
    // ── intagg ─────────────────────────────────────────────────────────
    ExtensionDef {
        name: "intagg",
        default_version: "1.1",
        versions: &[ExtensionVersion {
            version: "1.1",
            from: None,
            sql: script!("intagg--1.1.sql"),
        }],
    },
    // ── insert_username (SPI) ──────────────────────────────────────────
    ExtensionDef {
        name: "insert_username",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("insert_username--1.0.sql"),
        }],
    },
    // ── intarray ───────────────────────────────────────────────────────
    ExtensionDef {
        name: "intarray",
        default_version: "1.5",
        versions: &[
            ExtensionVersion {
                version: "1.2",
                from: None,
                sql: script!("intarray--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: script!("intarray--1.2--1.3.sql"),
            },
            ExtensionVersion {
                version: "1.4",
                from: Some("1.3"),
                sql: script!("intarray--1.3--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: script!("intarray--1.4--1.5.sql"),
            },
        ],
    },
    // ── isn ────────────────────────────────────────────────────────────
    ExtensionDef {
        name: "isn",
        default_version: "1.3",
        versions: &[
            ExtensionVersion {
                version: "1.1",
                from: None,
                sql: script!("isn--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: script!("isn--1.1--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: script!("isn--1.2--1.3.sql"),
            },
        ],
    },
    // ── jsonb_plperl (requires plperl) ─────────────────────────────────
    ExtensionDef {
        name: "jsonb_plperl",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("jsonb_plperl--1.0.sql"),
        }],
    },
    // ── jsonb_plperlu (requires plperlu) ───────────────────────────────
    ExtensionDef {
        name: "jsonb_plperlu",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("jsonb_plperlu--1.0.sql"),
        }],
    },
    // ── jsonb_plpython3u (requires plpython3u) ─────────────────────────
    ExtensionDef {
        name: "jsonb_plpython3u",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("jsonb_plpython3u--1.0.sql"),
        }],
    },
    // ── lo ─────────────────────────────────────────────────────────────
    ExtensionDef {
        name: "lo",
        default_version: "1.2",
        versions: &[
            ExtensionVersion {
                version: "1.1",
                from: None,
                sql: script!("lo--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: script!("lo--1.1--1.2.sql"),
            },
        ],
    },
    // ── ltree ──────────────────────────────────────────────────────────
    ExtensionDef {
        name: "ltree",
        default_version: "1.3",
        versions: &[
            ExtensionVersion {
                version: "1.1",
                from: None,
                sql: script!("ltree--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: script!("ltree--1.1--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: script!("ltree--1.2--1.3.sql"),
            },
        ],
    },
    // ── ltree_plpython3u (requires ltree + plpython3u) ─────────────────
    ExtensionDef {
        name: "ltree_plpython3u",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("ltree_plpython3u--1.0.sql"),
        }],
    },
    // ── moddatetime (SPI) ──────────────────────────────────────────────
    ExtensionDef {
        name: "moddatetime",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("moddatetime--1.0.sql"),
        }],
    },
    // ── pageinspect ────────────────────────────────────────────────────
    ExtensionDef {
        name: "pageinspect",
        default_version: "1.13",
        versions: &[
            ExtensionVersion {
                version: "1.5",
                from: None,
                sql: script!("pageinspect--1.5.sql"),
            },
            ExtensionVersion {
                version: "1.6",
                from: Some("1.5"),
                sql: script!("pageinspect--1.5--1.6.sql"),
            },
            ExtensionVersion {
                version: "1.7",
                from: Some("1.6"),
                sql: script!("pageinspect--1.6--1.7.sql"),
            },
            ExtensionVersion {
                version: "1.8",
                from: Some("1.7"),
                sql: script!("pageinspect--1.7--1.8.sql"),
            },
            ExtensionVersion {
                version: "1.9",
                from: Some("1.8"),
                sql: script!("pageinspect--1.8--1.9.sql"),
            },
            ExtensionVersion {
                version: "1.10",
                from: Some("1.9"),
                sql: script!("pageinspect--1.9--1.10.sql"),
            },
            ExtensionVersion {
                version: "1.11",
                from: Some("1.10"),
                sql: script!("pageinspect--1.10--1.11.sql"),
            },
            ExtensionVersion {
                version: "1.12",
                from: Some("1.11"),
                sql: script!("pageinspect--1.11--1.12.sql"),
            },
            ExtensionVersion {
                version: "1.13",
                from: Some("1.12"),
                sql: script!("pageinspect--1.12--1.13.sql"),
            },
        ],
    },
    // ── pg_buffercache ─────────────────────────────────────────────────
    ExtensionDef {
        name: "pg_buffercache",
        default_version: "1.7",
        versions: &[
            ExtensionVersion {
                version: "1.2",
                from: None,
                sql: script!("pg_buffercache--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: script!("pg_buffercache--1.2--1.3.sql"),
            },
            ExtensionVersion {
                version: "1.4",
                from: Some("1.3"),
                sql: script!("pg_buffercache--1.3--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: script!("pg_buffercache--1.4--1.5.sql"),
            },
            ExtensionVersion {
                version: "1.6",
                from: Some("1.5"),
                sql: script!("pg_buffercache--1.5--1.6.sql"),
            },
            ExtensionVersion {
                version: "1.7",
                from: Some("1.6"),
                sql: script!("pg_buffercache--1.6--1.7.sql"),
            },
        ],
    },
    // ── pg_freespacemap ────────────────────────────────────────────────
    ExtensionDef {
        name: "pg_freespacemap",
        default_version: "1.3",
        versions: &[
            ExtensionVersion {
                version: "1.1",
                from: None,
                sql: script!("pg_freespacemap--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: script!("pg_freespacemap--1.1--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: script!("pg_freespacemap--1.2--1.3.sql"),
            },
        ],
    },
    // ── pg_logicalinspect ──────────────────────────────────────────────
    ExtensionDef {
        name: "pg_logicalinspect",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("pg_logicalinspect--1.0.sql"),
        }],
    },
    // ── pg_prewarm ─────────────────────────────────────────────────────
    ExtensionDef {
        name: "pg_prewarm",
        default_version: "1.2",
        versions: &[
            ExtensionVersion {
                version: "1.1",
                from: None,
                sql: script!("pg_prewarm--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: script!("pg_prewarm--1.1--1.2.sql"),
            },
        ],
    },
    // ── pg_stash_advice ────────────────────────────────────────────────
    ExtensionDef {
        name: "pg_stash_advice",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("pg_stash_advice--1.0.sql"),
        }],
    },
    // ── pg_stat_statements ─────────────────────────────────────────────
    ExtensionDef {
        name: "pg_stat_statements",
        default_version: "1.13",
        versions: &[
            ExtensionVersion {
                version: "1.4",
                from: None,
                sql: script!("pg_stat_statements--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: script!("pg_stat_statements--1.4--1.5.sql"),
            },
            ExtensionVersion {
                version: "1.6",
                from: Some("1.5"),
                sql: script!("pg_stat_statements--1.5--1.6.sql"),
            },
            ExtensionVersion {
                version: "1.7",
                from: Some("1.6"),
                sql: script!("pg_stat_statements--1.6--1.7.sql"),
            },
            ExtensionVersion {
                version: "1.8",
                from: Some("1.7"),
                sql: script!("pg_stat_statements--1.7--1.8.sql"),
            },
            ExtensionVersion {
                version: "1.9",
                from: Some("1.8"),
                sql: script!("pg_stat_statements--1.8--1.9.sql"),
            },
            ExtensionVersion {
                version: "1.10",
                from: Some("1.9"),
                sql: script!("pg_stat_statements--1.9--1.10.sql"),
            },
            ExtensionVersion {
                version: "1.11",
                from: Some("1.10"),
                sql: script!("pg_stat_statements--1.10--1.11.sql"),
            },
            ExtensionVersion {
                version: "1.12",
                from: Some("1.11"),
                sql: script!("pg_stat_statements--1.11--1.12.sql"),
            },
            ExtensionVersion {
                version: "1.13",
                from: Some("1.12"),
                sql: script!("pg_stat_statements--1.12--1.13.sql"),
            },
        ],
    },
    // ── pg_surgery ─────────────────────────────────────────────────────
    ExtensionDef {
        name: "pg_surgery",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("pg_surgery--1.0.sql"),
        }],
    },
    // ── pg_trgm ────────────────────────────────────────────────────────
    ExtensionDef {
        name: "pg_trgm",
        default_version: "1.6",
        versions: &[
            ExtensionVersion {
                version: "1.3",
                from: None,
                sql: script!("pg_trgm--1.3.sql"),
            },
            ExtensionVersion {
                version: "1.4",
                from: Some("1.3"),
                sql: script!("pg_trgm--1.3--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: script!("pg_trgm--1.4--1.5.sql"),
            },
            ExtensionVersion {
                version: "1.6",
                from: Some("1.5"),
                sql: script!("pg_trgm--1.5--1.6.sql"),
            },
        ],
    },
    // ── pg_visibility ──────────────────────────────────────────────────
    ExtensionDef {
        name: "pg_visibility",
        default_version: "1.2",
        versions: &[
            ExtensionVersion {
                version: "1.1",
                from: None,
                sql: script!("pg_visibility--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: script!("pg_visibility--1.1--1.2.sql"),
            },
        ],
    },
    // ── pg_walinspect ──────────────────────────────────────────────────
    ExtensionDef {
        name: "pg_walinspect",
        default_version: "1.1",
        versions: &[
            ExtensionVersion {
                version: "1.0",
                from: None,
                sql: script!("pg_walinspect--1.0.sql"),
            },
            ExtensionVersion {
                version: "1.1",
                from: Some("1.0"),
                sql: script!("pg_walinspect--1.0--1.1.sql"),
            },
        ],
    },
    // ── pgcrypto ───────────────────────────────────────────────────────
    ExtensionDef {
        name: "pgcrypto",
        default_version: "1.4",
        versions: &[
            ExtensionVersion {
                version: "1.3",
                from: None,
                sql: script!("pgcrypto--1.3.sql"),
            },
            ExtensionVersion {
                version: "1.4",
                from: Some("1.3"),
                sql: script!("pgcrypto--1.3--1.4.sql"),
            },
        ],
    },
    // ── pgrowlocks ─────────────────────────────────────────────────────
    ExtensionDef {
        name: "pgrowlocks",
        default_version: "1.2",
        versions: &[ExtensionVersion {
            version: "1.2",
            from: None,
            sql: script!("pgrowlocks--1.2.sql"),
        }],
    },
    // ── pgstattuple ────────────────────────────────────────────────────
    ExtensionDef {
        name: "pgstattuple",
        default_version: "1.5",
        versions: &[
            ExtensionVersion {
                version: "1.4",
                from: None,
                sql: script!("pgstattuple--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: script!("pgstattuple--1.4--1.5.sql"),
            },
        ],
    },
    // ── plperl (src/pl/plperl) ─────────────────────────────────────────
    ExtensionDef {
        name: "plperl",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("plperl--1.0.sql"),
        }],
    },
    // ── plperlu (src/pl/plperl) ────────────────────────────────────────
    ExtensionDef {
        name: "plperlu",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("plperlu--1.0.sql"),
        }],
    },
    // ── plpgsql (src/pl/plpgsql, installed by initdb) ─────────────────
    ExtensionDef {
        name: "plpgsql",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("plpgsql--1.0.sql"),
        }],
    },
    // ── plpython3u (src/pl/plpython) ───────────────────────────────────
    ExtensionDef {
        name: "plpython3u",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("plpython3u--1.0.sql"),
        }],
    },
    // ── postgres_fdw ───────────────────────────────────────────────────
    ExtensionDef {
        name: "postgres_fdw",
        default_version: "1.3",
        versions: &[
            ExtensionVersion {
                version: "1.0",
                from: None,
                sql: script!("postgres_fdw--1.0.sql"),
            },
            ExtensionVersion {
                version: "1.1",
                from: Some("1.0"),
                sql: script!("postgres_fdw--1.0--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: script!("postgres_fdw--1.1--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: script!("postgres_fdw--1.2--1.3.sql"),
            },
        ],
    },
    // ── refint (SPI) ───────────────────────────────────────────────────
    ExtensionDef {
        name: "refint",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("refint--1.0.sql"),
        }],
    },
    // ── seg ────────────────────────────────────────────────────────────
    ExtensionDef {
        name: "seg",
        default_version: "1.4",
        versions: &[
            ExtensionVersion {
                version: "1.1",
                from: None,
                sql: script!("seg--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: script!("seg--1.1--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: script!("seg--1.2--1.3.sql"),
            },
            ExtensionVersion {
                version: "1.4",
                from: Some("1.3"),
                sql: script!("seg--1.3--1.4.sql"),
            },
        ],
    },
    // ── sslinfo ────────────────────────────────────────────────────────
    ExtensionDef {
        name: "sslinfo",
        default_version: "1.2",
        versions: &[ExtensionVersion {
            version: "1.2",
            from: None,
            sql: script!("sslinfo--1.2.sql"),
        }],
    },
    // ── tablefunc ──────────────────────────────────────────────────────
    ExtensionDef {
        name: "tablefunc",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("tablefunc--1.0.sql"),
        }],
    },
    // ── tcn ────────────────────────────────────────────────────────────
    ExtensionDef {
        name: "tcn",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("tcn--1.0.sql"),
        }],
    },
    // ── tsm_system_rows ────────────────────────────────────────────────
    ExtensionDef {
        name: "tsm_system_rows",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("tsm_system_rows--1.0.sql"),
        }],
    },
    // ── tsm_system_time ────────────────────────────────────────────────
    ExtensionDef {
        name: "tsm_system_time",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: script!("tsm_system_time--1.0.sql"),
        }],
    },
    // ── unaccent ───────────────────────────────────────────────────────
    ExtensionDef {
        name: "unaccent",
        default_version: "1.1",
        versions: &[ExtensionVersion {
            version: "1.1",
            from: None,
            sql: script!("unaccent--1.1.sql"),
        }],
    },
    // ── uuid-ossp ──────────────────────────────────────────────────────
    ExtensionDef {
        name: "uuid-ossp",
        default_version: "1.1",
        versions: &[ExtensionVersion {
            version: "1.1",
            from: None,
            sql: script!("uuid-ossp--1.1.sql"),
        }],
    },
    // ── xml2 ───────────────────────────────────────────────────────────
    ExtensionDef {
        name: "xml2",
        default_version: "1.2",
        versions: &[
            ExtensionVersion {
                version: "1.1",
                from: None,
                sql: script!("xml2--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: script!("xml2--1.1--1.2.sql"),
            },
        ],
    },
    // ═══════════════════════════════════════════════════════════════════════
    // Third-party extensions
    // ═══════════════════════════════════════════════════════════════════════
    // ── pg_cron (pg_catalog; needs shared_preload_libraries on a server) ───
    ExtensionDef {
        name: "pg_cron",
        default_version: "1.6",
        versions: &[
            ExtensionVersion {
                version: "1.0",
                from: None,
                sql: script!("pg_cron--1.0.sql"),
            },
            ExtensionVersion {
                version: "1.1",
                from: Some("1.0"),
                sql: script!("pg_cron--1.0--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: script!("pg_cron--1.1--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: script!("pg_cron--1.2--1.3.sql"),
            },
            ExtensionVersion {
                version: "1.4",
                from: Some("1.3"),
                sql: script!("pg_cron--1.3--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.4-1",
                from: Some("1.4"),
                sql: script!("pg_cron--1.4--1.4-1.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4-1"),
                sql: script!("pg_cron--1.4-1--1.5.sql"),
            },
            ExtensionVersion {
                version: "1.6",
                from: Some("1.5"),
                sql: script!("pg_cron--1.5--1.6.sql"),
            },
        ],
    },
    // ── pg_jsonschema ─────────────────────────────────────────────────
    ExtensionDef {
        name: "pg_jsonschema",
        default_version: "0.3.4",
        versions: &[ExtensionVersion {
            version: "0.3.4",
            from: None,
            sql: script!("pg_jsonschema--0.3.4.sql"),
        }],
    },
    // ── pgmq (its release installs 1.13.2 only) ─────────────────────────
    ExtensionDef {
        name: "pgmq",
        default_version: "1.13.2",
        versions: &[ExtensionVersion {
            version: "1.13.2",
            from: None,
            sql: script!("pgmq--1.13.2.sql"),
        }],
    },
    // ── postgis ───────────────────────────────────────────────────────
    ExtensionDef {
        name: "postgis",
        default_version: "3.7.0",
        versions: &[
            ExtensionVersion {
                version: "3.7.0",
                from: None,
                sql: script!("postgis--3.7.0.sql"),
            },
            // The pseudo-versions: `unpackaged` installs nothing, `ANY`
            // only tags the version.
            ExtensionVersion {
                version: "unpackaged",
                from: None,
                sql: script!("postgis--unpackaged.sql"),
            },
            ExtensionVersion {
                version: "ANY",
                from: Some("3.7.0"),
                sql: script!("postgis--3.7.0--ANY.sql"),
            },
            // Back to 3.7.0 (see the comment above `REGISTRY`).
            ExtensionVersion {
                version: "3.7.0",
                from: Some("ANY"),
                sql: Script::Text(""),
            },
        ],
    },
    // ── postgis-3: a control file alone, which nothing installs ──────────
    ExtensionDef {
        name: "postgis-3",
        default_version: "3.7.0",
        versions: &[],
    },
    // ── postgis_raster ────────────────────────────────────────────────
    ExtensionDef {
        name: "postgis_raster",
        default_version: "3.7.0",
        versions: &[
            ExtensionVersion {
                version: "3.7.0",
                from: None,
                sql: script!("postgis_raster--3.7.0.sql"),
            },
            // The pseudo-versions: `unpackaged` installs nothing, `ANY`
            // only tags the version.
            ExtensionVersion {
                version: "unpackaged",
                from: None,
                sql: script!("postgis_raster--unpackaged.sql"),
            },
            ExtensionVersion {
                version: "ANY",
                from: Some("3.7.0"),
                sql: script!("postgis_raster--3.7.0--ANY.sql"),
            },
            // Back to 3.7.0 (see the comment above `REGISTRY`).
            ExtensionVersion {
                version: "3.7.0",
                from: Some("ANY"),
                sql: Script::Text(""),
            },
        ],
    },
    // ── postgis_raster-3: a control file alone, which nothing installs ──────────
    ExtensionDef {
        name: "postgis_raster-3",
        default_version: "3.7.0",
        versions: &[],
    },
    // ── postgis_sfcgal ────────────────────────────────────────────────
    ExtensionDef {
        name: "postgis_sfcgal",
        default_version: "3.7.0",
        versions: &[
            ExtensionVersion {
                version: "3.7.0",
                from: None,
                sql: script!("postgis_sfcgal--3.7.0.sql"),
            },
            // The pseudo-versions: `unpackaged` installs nothing, `ANY`
            // only tags the version.
            ExtensionVersion {
                version: "unpackaged",
                from: None,
                sql: script!("postgis_sfcgal--unpackaged.sql"),
            },
            ExtensionVersion {
                version: "ANY",
                from: Some("3.7.0"),
                sql: script!("postgis_sfcgal--3.7.0--ANY.sql"),
            },
            // Back to 3.7.0 (see the comment above `REGISTRY`).
            ExtensionVersion {
                version: "3.7.0",
                from: Some("ANY"),
                sql: Script::Text(""),
            },
        ],
    },
    // ── postgis_sfcgal-3: a control file alone, which nothing installs ──────────
    ExtensionDef {
        name: "postgis_sfcgal-3",
        default_version: "3.7.0",
        versions: &[],
    },
    // ── postgis_topology ──────────────────────────────────────────────
    ExtensionDef {
        name: "postgis_topology",
        default_version: "3.7.0",
        versions: &[
            ExtensionVersion {
                version: "3.7.0",
                from: None,
                sql: script!("postgis_topology--3.7.0.sql"),
            },
            // The pseudo-versions: `unpackaged` installs nothing, `ANY`
            // only tags the version.
            ExtensionVersion {
                version: "unpackaged",
                from: None,
                sql: script!("postgis_topology--unpackaged.sql"),
            },
            ExtensionVersion {
                version: "ANY",
                from: Some("3.7.0"),
                sql: script!("postgis_topology--3.7.0--ANY.sql"),
            },
            // Back to 3.7.0 (see the comment above `REGISTRY`).
            ExtensionVersion {
                version: "3.7.0",
                from: Some("ANY"),
                sql: Script::Text(""),
            },
        ],
    },
    // ── postgis_topology-3: a control file alone, which nothing installs ──────────
    ExtensionDef {
        name: "postgis_topology-3",
        default_version: "3.7.0",
        versions: &[],
    },
    // ── vector (pgvector) ──────────────────────────────────────────────
    // What the pgvector/pgvector:pg19 image ships: 0.8.7's install script,
    // no older version and no update script.
    ExtensionDef {
        name: "vector",
        default_version: "0.8.7",
        versions: &[ExtensionVersion {
            version: "0.8.7",
            from: None,
            sql: script!("vector--0.8.7.sql"),
        }],
    },
];
