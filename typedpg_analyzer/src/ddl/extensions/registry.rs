//! The extension registry: every supported extension, its default
//! version, and the SQL script chain (base install + upgrades).
//!
//! This is pure data — the install/upgrade logic lives in the parent
//! `extensions` module. Child modules can name the parent's private
//! `ExtensionDef`/`ExtensionVersion` structs, so they stay defined there.

use super::{ExtensionDef, ExtensionVersion};

// ─── Registry ───────────────────────────────────────────────────────────────
//
// All PostgreSQL contrib extensions, alphabetical order.
// SQL files are in typedpg_analyzer/src/extensions/ and fetched via
// update_extensions.sh from the PostgreSQL repository.

pub(super) static REGISTRY: &[ExtensionDef] = &[
    // ── amcheck ────────────────────────────────────────────────────────
    ExtensionDef {
        name: "amcheck",
        default_version: "1.5",
        versions: &[
            ExtensionVersion {
                version: "1.0",
                from: None,
                sql: include_str!("../../extensions/amcheck--1.0.sql"),
            },
            ExtensionVersion {
                version: "1.1",
                from: Some("1.0"),
                sql: include_str!("../../extensions/amcheck--1.0--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: include_str!("../../extensions/amcheck--1.1--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: include_str!("../../extensions/amcheck--1.2--1.3.sql"),
            },
            ExtensionVersion {
                version: "1.4",
                from: Some("1.3"),
                sql: include_str!("../../extensions/amcheck--1.3--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: include_str!("../../extensions/amcheck--1.4--1.5.sql"),
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
            sql: include_str!("../../extensions/autoinc--1.0.sql"),
        }],
    },
    // ── bloom ──────────────────────────────────────────────────────────
    ExtensionDef {
        name: "bloom",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../../extensions/bloom--1.0.sql"),
        }],
    },
    // ── bool_plperl (requires plperl) ──────────────────────────────────
    ExtensionDef {
        name: "bool_plperl",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../../extensions/bool_plperl--1.0.sql"),
        }],
    },
    // ── bool_plperlu (requires plperlu) ────────────────────────────────
    ExtensionDef {
        name: "bool_plperlu",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../../extensions/bool_plperlu--1.0.sql"),
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
                sql: include_str!("../../extensions/btree_gin--1.0.sql"),
            },
            ExtensionVersion {
                version: "1.1",
                from: Some("1.0"),
                sql: include_str!("../../extensions/btree_gin--1.0--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: include_str!("../../extensions/btree_gin--1.1--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: include_str!("../../extensions/btree_gin--1.2--1.3.sql"),
            },
            ExtensionVersion {
                version: "1.4",
                from: Some("1.3"),
                sql: include_str!("../../extensions/btree_gin--1.3--1.4.sql"),
            },
        ],
    },
    // ── btree_gist ─────────────────────────────────────────────────────
    ExtensionDef {
        name: "btree_gist",
        default_version: "1.9",
        versions: &[ExtensionVersion {
            version: "1.9",
            from: None,
            sql: include_str!("../../extensions/btree_gist--1.9.sql"),
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
                sql: include_str!("../../extensions/citext--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: include_str!("../../extensions/citext--1.4--1.5.sql"),
            },
            ExtensionVersion {
                version: "1.6",
                from: Some("1.5"),
                sql: include_str!("../../extensions/citext--1.5--1.6.sql"),
            },
            ExtensionVersion {
                version: "1.7",
                from: Some("1.6"),
                sql: include_str!("../../extensions/citext--1.6--1.7.sql"),
            },
            ExtensionVersion {
                version: "1.8",
                from: Some("1.7"),
                sql: include_str!("../../extensions/citext--1.7--1.8.sql"),
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
                sql: include_str!("../../extensions/cube--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: include_str!("../../extensions/cube--1.2--1.3.sql"),
            },
            ExtensionVersion {
                version: "1.4",
                from: Some("1.3"),
                sql: include_str!("../../extensions/cube--1.3--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: include_str!("../../extensions/cube--1.4--1.5.sql"),
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
            sql: include_str!("../../extensions/dblink--1.2.sql"),
        }],
    },
    // ── dict_int ───────────────────────────────────────────────────────
    ExtensionDef {
        name: "dict_int",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../../extensions/dict_int--1.0.sql"),
        }],
    },
    // ── dict_xsyn ──────────────────────────────────────────────────────
    ExtensionDef {
        name: "dict_xsyn",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../../extensions/dict_xsyn--1.0.sql"),
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
                sql: include_str!("../../extensions/earthdistance--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: include_str!("../../extensions/earthdistance--1.1--1.2.sql"),
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
            sql: include_str!("../../extensions/file_fdw--1.0.sql"),
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
                sql: include_str!("../../extensions/fuzzystrmatch--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: include_str!("../../extensions/fuzzystrmatch--1.1--1.2.sql"),
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
                sql: include_str!("../../extensions/hstore--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: include_str!("../../extensions/hstore--1.4--1.5.sql"),
            },
            ExtensionVersion {
                version: "1.6",
                from: Some("1.5"),
                sql: include_str!("../../extensions/hstore--1.5--1.6.sql"),
            },
            ExtensionVersion {
                version: "1.7",
                from: Some("1.6"),
                sql: include_str!("../../extensions/hstore--1.6--1.7.sql"),
            },
            ExtensionVersion {
                version: "1.8",
                from: Some("1.7"),
                sql: include_str!("../../extensions/hstore--1.7--1.8.sql"),
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
            sql: include_str!("../../extensions/hstore_plperl--1.0.sql"),
        }],
    },
    // ── hstore_plperlu (requires hstore + plperlu) ─────────────────────
    ExtensionDef {
        name: "hstore_plperlu",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../../extensions/hstore_plperlu--1.0.sql"),
        }],
    },
    // ── hstore_plpython3u (requires hstore + plpython3u) ───────────────
    ExtensionDef {
        name: "hstore_plpython3u",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../../extensions/hstore_plpython3u--1.0.sql"),
        }],
    },
    // ── intagg ─────────────────────────────────────────────────────────
    ExtensionDef {
        name: "intagg",
        default_version: "1.1",
        versions: &[ExtensionVersion {
            version: "1.1",
            from: None,
            sql: include_str!("../../extensions/intagg--1.1.sql"),
        }],
    },
    // ── insert_username (SPI) ──────────────────────────────────────────
    ExtensionDef {
        name: "insert_username",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../../extensions/insert_username--1.0.sql"),
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
                sql: include_str!("../../extensions/intarray--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: include_str!("../../extensions/intarray--1.2--1.3.sql"),
            },
            ExtensionVersion {
                version: "1.4",
                from: Some("1.3"),
                sql: include_str!("../../extensions/intarray--1.3--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: include_str!("../../extensions/intarray--1.4--1.5.sql"),
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
                sql: include_str!("../../extensions/isn--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: include_str!("../../extensions/isn--1.1--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: include_str!("../../extensions/isn--1.2--1.3.sql"),
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
            sql: include_str!("../../extensions/jsonb_plperl--1.0.sql"),
        }],
    },
    // ── jsonb_plperlu (requires plperlu) ───────────────────────────────
    ExtensionDef {
        name: "jsonb_plperlu",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../../extensions/jsonb_plperlu--1.0.sql"),
        }],
    },
    // ── jsonb_plpython3u (requires plpython3u) ─────────────────────────
    ExtensionDef {
        name: "jsonb_plpython3u",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../../extensions/jsonb_plpython3u--1.0.sql"),
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
                sql: include_str!("../../extensions/lo--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: include_str!("../../extensions/lo--1.1--1.2.sql"),
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
                sql: include_str!("../../extensions/ltree--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: include_str!("../../extensions/ltree--1.1--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: include_str!("../../extensions/ltree--1.2--1.3.sql"),
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
            sql: include_str!("../../extensions/ltree_plpython3u--1.0.sql"),
        }],
    },
    // ── moddatetime (SPI) ──────────────────────────────────────────────
    ExtensionDef {
        name: "moddatetime",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../../extensions/moddatetime--1.0.sql"),
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
                sql: include_str!("../../extensions/pageinspect--1.5.sql"),
            },
            ExtensionVersion {
                version: "1.6",
                from: Some("1.5"),
                sql: include_str!("../../extensions/pageinspect--1.5--1.6.sql"),
            },
            ExtensionVersion {
                version: "1.7",
                from: Some("1.6"),
                sql: include_str!("../../extensions/pageinspect--1.6--1.7.sql"),
            },
            ExtensionVersion {
                version: "1.8",
                from: Some("1.7"),
                sql: include_str!("../../extensions/pageinspect--1.7--1.8.sql"),
            },
            ExtensionVersion {
                version: "1.9",
                from: Some("1.8"),
                sql: include_str!("../../extensions/pageinspect--1.8--1.9.sql"),
            },
            ExtensionVersion {
                version: "1.10",
                from: Some("1.9"),
                sql: include_str!("../../extensions/pageinspect--1.9--1.10.sql"),
            },
            ExtensionVersion {
                version: "1.11",
                from: Some("1.10"),
                sql: include_str!("../../extensions/pageinspect--1.10--1.11.sql"),
            },
            ExtensionVersion {
                version: "1.12",
                from: Some("1.11"),
                sql: include_str!("../../extensions/pageinspect--1.11--1.12.sql"),
            },
            ExtensionVersion {
                version: "1.13",
                from: Some("1.12"),
                sql: include_str!("../../extensions/pageinspect--1.12--1.13.sql"),
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
                sql: include_str!("../../extensions/pg_buffercache--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: include_str!("../../extensions/pg_buffercache--1.2--1.3.sql"),
            },
            ExtensionVersion {
                version: "1.4",
                from: Some("1.3"),
                sql: include_str!("../../extensions/pg_buffercache--1.3--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: include_str!("../../extensions/pg_buffercache--1.4--1.5.sql"),
            },
            ExtensionVersion {
                version: "1.6",
                from: Some("1.5"),
                sql: include_str!("../../extensions/pg_buffercache--1.5--1.6.sql"),
            },
            ExtensionVersion {
                version: "1.7",
                from: Some("1.6"),
                sql: include_str!("../../extensions/pg_buffercache--1.6--1.7.sql"),
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
                sql: include_str!("../../extensions/pg_freespacemap--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: include_str!("../../extensions/pg_freespacemap--1.1--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: include_str!("../../extensions/pg_freespacemap--1.2--1.3.sql"),
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
            sql: include_str!("../../extensions/pg_logicalinspect--1.0.sql"),
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
                sql: include_str!("../../extensions/pg_prewarm--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: include_str!("../../extensions/pg_prewarm--1.1--1.2.sql"),
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
            sql: include_str!("../../extensions/pg_stash_advice--1.0.sql"),
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
                sql: include_str!("../../extensions/pg_stat_statements--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: include_str!("../../extensions/pg_stat_statements--1.4--1.5.sql"),
            },
            ExtensionVersion {
                version: "1.6",
                from: Some("1.5"),
                sql: include_str!("../../extensions/pg_stat_statements--1.5--1.6.sql"),
            },
            ExtensionVersion {
                version: "1.7",
                from: Some("1.6"),
                sql: include_str!("../../extensions/pg_stat_statements--1.6--1.7.sql"),
            },
            ExtensionVersion {
                version: "1.8",
                from: Some("1.7"),
                sql: include_str!("../../extensions/pg_stat_statements--1.7--1.8.sql"),
            },
            ExtensionVersion {
                version: "1.9",
                from: Some("1.8"),
                sql: include_str!("../../extensions/pg_stat_statements--1.8--1.9.sql"),
            },
            ExtensionVersion {
                version: "1.10",
                from: Some("1.9"),
                sql: include_str!("../../extensions/pg_stat_statements--1.9--1.10.sql"),
            },
            ExtensionVersion {
                version: "1.11",
                from: Some("1.10"),
                sql: include_str!("../../extensions/pg_stat_statements--1.10--1.11.sql"),
            },
            ExtensionVersion {
                version: "1.12",
                from: Some("1.11"),
                sql: include_str!("../../extensions/pg_stat_statements--1.11--1.12.sql"),
            },
            ExtensionVersion {
                version: "1.13",
                from: Some("1.12"),
                sql: include_str!("../../extensions/pg_stat_statements--1.12--1.13.sql"),
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
            sql: include_str!("../../extensions/pg_surgery--1.0.sql"),
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
                sql: include_str!("../../extensions/pg_trgm--1.3.sql"),
            },
            ExtensionVersion {
                version: "1.4",
                from: Some("1.3"),
                sql: include_str!("../../extensions/pg_trgm--1.3--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: include_str!("../../extensions/pg_trgm--1.4--1.5.sql"),
            },
            ExtensionVersion {
                version: "1.6",
                from: Some("1.5"),
                sql: include_str!("../../extensions/pg_trgm--1.5--1.6.sql"),
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
                sql: include_str!("../../extensions/pg_visibility--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: include_str!("../../extensions/pg_visibility--1.1--1.2.sql"),
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
                sql: include_str!("../../extensions/pg_walinspect--1.0.sql"),
            },
            ExtensionVersion {
                version: "1.1",
                from: Some("1.0"),
                sql: include_str!("../../extensions/pg_walinspect--1.0--1.1.sql"),
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
                sql: include_str!("../../extensions/pgcrypto--1.3.sql"),
            },
            ExtensionVersion {
                version: "1.4",
                from: Some("1.3"),
                sql: include_str!("../../extensions/pgcrypto--1.3--1.4.sql"),
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
            sql: include_str!("../../extensions/pgrowlocks--1.2.sql"),
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
                sql: include_str!("../../extensions/pgstattuple--1.4.sql"),
            },
            ExtensionVersion {
                version: "1.5",
                from: Some("1.4"),
                sql: include_str!("../../extensions/pgstattuple--1.4--1.5.sql"),
            },
        ],
    },
    // ── plpgsql (src/pl/plpgsql, installed by initdb) ─────────────────
    ExtensionDef {
        name: "plpgsql",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../../extensions/plpgsql--1.0.sql"),
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
                sql: include_str!("../../extensions/postgres_fdw--1.0.sql"),
            },
            ExtensionVersion {
                version: "1.1",
                from: Some("1.0"),
                sql: include_str!("../../extensions/postgres_fdw--1.0--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: include_str!("../../extensions/postgres_fdw--1.1--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: include_str!("../../extensions/postgres_fdw--1.2--1.3.sql"),
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
            sql: include_str!("../../extensions/refint--1.0.sql"),
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
                sql: include_str!("../../extensions/seg--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: include_str!("../../extensions/seg--1.1--1.2.sql"),
            },
            ExtensionVersion {
                version: "1.3",
                from: Some("1.2"),
                sql: include_str!("../../extensions/seg--1.2--1.3.sql"),
            },
            ExtensionVersion {
                version: "1.4",
                from: Some("1.3"),
                sql: include_str!("../../extensions/seg--1.3--1.4.sql"),
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
            sql: include_str!("../../extensions/sslinfo--1.2.sql"),
        }],
    },
    // ── tablefunc ──────────────────────────────────────────────────────
    ExtensionDef {
        name: "tablefunc",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../../extensions/tablefunc--1.0.sql"),
        }],
    },
    // ── tcn ────────────────────────────────────────────────────────────
    ExtensionDef {
        name: "tcn",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../../extensions/tcn--1.0.sql"),
        }],
    },
    // ── tsm_system_rows ────────────────────────────────────────────────
    ExtensionDef {
        name: "tsm_system_rows",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../../extensions/tsm_system_rows--1.0.sql"),
        }],
    },
    // ── tsm_system_time ────────────────────────────────────────────────
    ExtensionDef {
        name: "tsm_system_time",
        default_version: "1.0",
        versions: &[ExtensionVersion {
            version: "1.0",
            from: None,
            sql: include_str!("../../extensions/tsm_system_time--1.0.sql"),
        }],
    },
    // ── unaccent ───────────────────────────────────────────────────────
    ExtensionDef {
        name: "unaccent",
        default_version: "1.1",
        versions: &[ExtensionVersion {
            version: "1.1",
            from: None,
            sql: include_str!("../../extensions/unaccent--1.1.sql"),
        }],
    },
    // ── uuid-ossp ──────────────────────────────────────────────────────
    ExtensionDef {
        name: "uuid-ossp",
        default_version: "1.1",
        versions: &[ExtensionVersion {
            version: "1.1",
            from: None,
            sql: include_str!("../../extensions/uuid-ossp--1.1.sql"),
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
                sql: include_str!("../../extensions/xml2--1.1.sql"),
            },
            ExtensionVersion {
                version: "1.2",
                from: Some("1.1"),
                sql: include_str!("../../extensions/xml2--1.1--1.2.sql"),
            },
        ],
    },
    // ═══════════════════════════════════════════════════════════════════════
    // Third-party extensions
    // ═══════════════════════════════════════════════════════════════════════
    // ── vector (pgvector) ──────────────────────────────────────────────
    ExtensionDef {
        name: "vector",
        default_version: "0.8.2",
        versions: &[ExtensionVersion {
            version: "0.8.2",
            from: None,
            sql: include_str!("../../extensions/vector--0.8.2.sql"),
        }],
    },
];
