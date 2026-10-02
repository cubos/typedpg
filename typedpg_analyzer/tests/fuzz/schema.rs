//! Fixed base schema, name/operator/function pools, and the random
//! schema generator the fuzzer extends the base with.

use super::*;

// ──────────────────────────────────────────────────────────────────────────
// Schema the fuzzer generates queries against.
// ──────────────────────────────────────────────────────────────────────────

pub(crate) const SETUP_SQL: &str = "
CREATE TYPE status AS ENUM ('draft', 'published', 'archived');
CREATE DOMAIN email AS TEXT;
CREATE TABLE users (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name       TEXT NOT NULL,
    addr       email,
    age        INT,
    score      NUMERIC(10, 2),
    active     BOOL NOT NULL DEFAULT true,
    ratio      FLOAT8,
    tags       TEXT[],
    nums       INT[],
    st         status NOT NULL DEFAULT 'draft',
    prefs      JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    bday       DATE,
    uid        UUID
);
CREATE TABLE posts (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id      BIGINT NOT NULL REFERENCES users(id),
    title        TEXT NOT NULL,
    body         TEXT,
    views        INT NOT NULL DEFAULT 0,
    rating       FLOAT8,
    published_at TIMESTAMPTZ
);
CREATE TABLE payments (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id     BIGINT NOT NULL REFERENCES users(id),
    reviewer_id BIGINT REFERENCES users(id),
    method      TEXT NOT NULL,
    card_last4  TEXT,
    iban        TEXT,
    CHECK ((method = 'a' AND card_last4 IS NOT NULL AND iban IS NULL)
        OR (method = 'b' AND iban IS NOT NULL AND card_last4 IS NULL)
        OR method NOT IN ('a', 'b')),
    CHECK (num_nonnulls(card_last4, iban) <= 1)
);
";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ty {
    Int,
    BigInt,
    Numeric,
    Float,
    Text,
    Bool,
    Timestamptz,
    Date,
    Uuid,
    Jsonb,
    Enum,
    IntArr,
    TextArr,
}

pub(crate) struct Col {
    pub(crate) name: &'static str,
    pub(crate) ty: Ty,
}

pub(crate) struct Table {
    pub(crate) name: &'static str,
    pub(crate) cols: &'static [Col],
}

pub(crate) const TABLES: &[Table] = &[
    Table {
        name: "users",
        cols: &[
            Col {
                name: "id",
                ty: Ty::BigInt,
            },
            Col {
                name: "name",
                ty: Ty::Text,
            },
            Col {
                name: "addr",
                ty: Ty::Text,
            },
            Col {
                name: "age",
                ty: Ty::Int,
            },
            Col {
                name: "score",
                ty: Ty::Numeric,
            },
            Col {
                name: "active",
                ty: Ty::Bool,
            },
            Col {
                name: "ratio",
                ty: Ty::Float,
            },
            Col {
                name: "tags",
                ty: Ty::TextArr,
            },
            Col {
                name: "nums",
                ty: Ty::IntArr,
            },
            Col {
                name: "st",
                ty: Ty::Enum,
            },
            Col {
                name: "prefs",
                ty: Ty::Jsonb,
            },
            Col {
                name: "created_at",
                ty: Ty::Timestamptz,
            },
            Col {
                name: "bday",
                ty: Ty::Date,
            },
            Col {
                name: "uid",
                ty: Ty::Uuid,
            },
        ],
    },
    Table {
        name: "posts",
        cols: &[
            Col {
                name: "id",
                ty: Ty::BigInt,
            },
            Col {
                name: "user_id",
                ty: Ty::BigInt,
            },
            Col {
                name: "title",
                ty: Ty::Text,
            },
            Col {
                name: "body",
                ty: Ty::Text,
            },
            Col {
                name: "views",
                ty: Ty::Int,
            },
            Col {
                name: "rating",
                ty: Ty::Float,
            },
            Col {
                name: "published_at",
                ty: Ty::Timestamptz,
            },
        ],
    },
];

/// A table with foreign keys into `users` and CHECK constraints tying its
/// columns together — only the narrowing generator queries it.
pub(crate) const PAYMENTS: Table = Table {
    name: "payments",
    cols: &[
        Col {
            name: "id",
            ty: Ty::BigInt,
        },
        Col {
            name: "user_id",
            ty: Ty::BigInt,
        },
        Col {
            name: "reviewer_id",
            ty: Ty::BigInt,
        },
        Col {
            name: "method",
            ty: Ty::Text,
        },
        Col {
            name: "card_last4",
            ty: Ty::Text,
        },
        Col {
            name: "iban",
            ty: Ty::Text,
        },
    ],
};

/// Every column name across the schema — used by the AST mutator to swap one
/// column reference for another (often of a different type).
pub(crate) const ALL_COLUMN_NAMES: &[&str] = &[
    "id",
    "name",
    "addr",
    "age",
    "score",
    "active",
    "ratio",
    "tags",
    "nums",
    "st",
    "prefs",
    "created_at",
    "bday",
    "uid",
    "user_id",
    "title",
    "body",
    "views",
    "rating",
    "published_at",
];

pub(crate) const OPERATORS: &[&str] = &[
    "=", "<>", "<", ">", "<=", ">=", "+", "-", "*", "/", "%", "^", "||", "->", "->>", "#>", "#>>",
    "@>", "<@", "?", "&&", "|", "&", "#", "<<", ">>", "~", "~~", "!~~", "~*",
];

/// Functions with assorted signatures — calling them on the wrong argument
/// types is exactly what surfaces error-message divergences.
pub(crate) const FUNCTIONS: &[&str] = &[
    // string
    "length",
    "upper",
    "lower",
    "char_length",
    "octet_length",
    "trim",
    "btrim",
    "ltrim",
    "rtrim",
    "substr",
    "substring",
    "replace",
    "split_part",
    "left",
    "right",
    "lpad",
    "rpad",
    "reverse",
    "concat",
    "concat_ws",
    "format",
    "md5",
    "starts_with",
    "to_char",
    // numeric
    "abs",
    "round",
    "ceil",
    "floor",
    "trunc",
    "sqrt",
    "cbrt",
    "exp",
    "ln",
    "log",
    "power",
    "sign",
    "mod",
    "div",
    "gcd",
    "lcm",
    "width_bucket",
    // date/time
    "date_trunc",
    "date_part",
    "extract",
    "age",
    "now",
    "to_timestamp",
    "to_date",
    "make_date",
    // conditional / misc
    "coalesce",
    "nullif",
    "greatest",
    "least",
    // array
    "cardinality",
    "array_length",
    "array_upper",
    "array_lower",
    "array_ndims",
    "array_append",
    "array_cat",
    "array_remove",
    "array_position",
    "array_to_string",
    "unnest",
    // json/jsonb
    "jsonb_typeof",
    "json_typeof",
    "jsonb_array_length",
    "jsonb_object_keys",
    "jsonb_build_array",
    "jsonb_build_object",
    "to_jsonb",
    "jsonb_strip_nulls",
    "jsonb_pretty",
    // aggregates (so grouped queries and misuse get exercised)
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "string_agg",
    "array_agg",
    "bool_and",
    "bool_or",
];

pub(crate) const BASE_TYPE_NAMES: &[&str] = &[
    "int4",
    "int8",
    "int2",
    "numeric",
    "float8",
    "text",
    "varchar",
    "bool",
    "date",
    "timestamptz",
    "uuid",
    "jsonb",
    "bytea",
];

/// Cast targets for the literal-probe generator (Strategy 5): the base types
/// plus types whose *input syntax* the analyzer validates (or deliberately
/// doesn't) — ranges, arrays, json, datetimes, geometrics, reg*, and the
/// schema's own enum/domain. Probing `'<content>'::<type>` across this
/// surface checks both rejection wording and acceptance agreement.
pub(crate) const PROBE_TYPE_NAMES: &[&str] = &[
    "int2",
    "int4",
    "int8",
    "oid",
    "float4",
    "float8",
    "numeric",
    "bool",
    "uuid",
    "json",
    "jsonb",
    "int4range",
    "numrange",
    "tstzrange",
    "int4[]",
    "text[]",
    "date",
    "time",
    "timetz",
    "timestamp",
    "timestamptz",
    "interval",
    "macaddr",
    "inet",
    "point",
    "box",
    "bit",
    "varbit",
    "money",
    "regclass",
    "regtype",
    "regproc",
    "bytea",
    "text",
    "status", // enum from the fixed schema
    "email",  // domain over text
];

/// Literal contents for the probe generator — a mix of valid and invalid
/// inputs per type family: integer radix/underscore forms (PG 16+), float
/// specials, malformed numerics, array/range/json shapes, uuid variants,
/// datetime keywords, enum labels. Every entry is interesting against
/// *several* of the types above.
pub(crate) const LITERAL_PROBES: &[&str] = &[
    "",
    " ",
    "x",
    "hello",
    "42",
    "-7",
    " 42 ",
    "+42",
    "- 42",
    "0x1F",
    "0o17",
    "0b101",
    "1_000",
    "1__0",
    "1_",
    "_1",
    "42abc",
    "2147483648",
    "9999999999999999999999",
    "3.14",
    ".5",
    "5.",
    "1e3",
    "1e",
    "1.2.3",
    "NaN",
    "inf",
    "-Infinity",
    "t",
    "tr",
    "ye",
    "of",
    "o",
    "10",
    "{}",
    "{1,2}",
    "{\"a\": 1}",
    "[1,2]",
    "[1,2)",
    "(1,2]",
    "empty",
    " EMPTY ",
    "[1,]",
    "01",
    "nullx",
    "1.5e3",
    "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
    "a0eebc999c0b4ef8bb6d6bb9bd380a11",
    "a0-eebc999c0b4ef8bb6d6bb9bd380a11",
    "now",
    "2024-01-01",
    "1 day",
    "draft",
    "published",
    "bogus_label",
    "users",
    "no_such_relation",
];

// ──────────────────────────────────────────────────────────────────────────
// Schema fuzzing — a seeded, additive random schema (valid by construction).
//
// Extends the fixed base (users/posts) per run with domains, composites,
// multi-dimensional arrays, typmod'd columns (`varchar(n)`, `numeric(p,s)`,
// `char(n)`), generated columns, a view, and a second schema. The type-directed
// generator picks all of this up automatically via `iter_relations` /
// `iter_types`, so each seed explores a different type surface — exactly the
// corners (typmod propagation, composite fields, view nullability,
// cross-schema resolution) where PG's semantics get hairy. Applied through the
// non-panicking `apply_sql_checked`, so a DDL disagreement is itself a finding.
// ──────────────────────────────────────────────────────────────────────────

/// Build an additive random schema string. Objects are emitted in dependency
/// order (enums → domains → composites → tables → views → second schema) and
/// named with `en_/dom_/cmp_/r_/v_/s1` prefixes so they never collide with the
/// base `users`/`posts`.
pub(crate) fn gen_random_schema(rng: &mut StdRng) -> String {
    const BASES: &[&str] = &[
        "int4",
        "int8",
        "int2",
        "numeric",
        "float8",
        "text",
        "bool",
        "timestamptz",
        "date",
        "uuid",
        "jsonb",
    ];
    // A scalar type rendering, sometimes with a typmod — the typmod corners
    // (length/precision propagation) are a rich source of divergences.
    fn scalar_ty(rng: &mut StdRng) -> String {
        match rng.random_range(0..7) {
            0 => "varchar(8)".into(),
            1 => "numeric(10, 2)".into(),
            2 => "char(4)".into(),
            3 => "varchar".into(),
            _ => BASES[rng.random_range(0..BASES.len())].into(),
        }
    }

    let mut s = String::new();
    let n_en = rng.random_range(0..=2);
    for i in 0..n_en {
        s.push_str(&format!("CREATE TYPE en_{i} AS ENUM ('a', 'b', 'c');\n"));
    }
    let n_dom = rng.random_range(0..=2);
    for i in 0..n_dom {
        let base = scalar_ty(rng);
        let extra = if rng.random_bool(0.4) {
            " NOT NULL"
        } else {
            ""
        };
        s.push_str(&format!("CREATE DOMAIN dom_{i} AS {base}{extra};\n"));
    }
    let n_cmp = rng.random_range(0..=2);
    for i in 0..n_cmp {
        s.push_str(&format!(
            "CREATE TYPE cmp_{i} AS (f0 {}, f1 {});\n",
            scalar_ty(rng),
            scalar_ty(rng),
        ));
    }

    // A column type drawn from the full palette: scalars/typmod, arrays,
    // multi-dim arrays, and any user types created above.
    let col_ty = |rng: &mut StdRng| -> String {
        let mut choices = vec![
            scalar_ty(rng),
            format!("{}[]", BASES[rng.random_range(0..BASES.len())]),
            "int4[][]".into(),
        ];
        if n_en > 0 {
            choices.push(format!("en_{}", rng.random_range(0..n_en)));
        }
        if n_dom > 0 {
            choices.push(format!("dom_{}", rng.random_range(0..n_dom)));
        }
        if n_cmp > 0 {
            choices.push(format!("cmp_{}", rng.random_range(0..n_cmp)));
        }
        choices[rng.random_range(0..choices.len())].clone()
    };

    let n_tbl = rng.random_range(1..=3);
    for i in 0..n_tbl {
        let mut cols = vec!["id BIGINT PRIMARY KEY".to_string()];
        for c in 0..rng.random_range(2..=5) {
            let nn = if rng.random_bool(0.4) {
                " NOT NULL"
            } else {
                ""
            };
            cols.push(format!("c{c} {}{nn}", col_ty(rng)));
        }
        if rng.random_bool(0.4) {
            cols.push("g BIGINT GENERATED ALWAYS AS (id * 2) STORED".into());
        }
        s.push_str(&format!(
            "CREATE TABLE r_{i} (\n  {}\n);\n",
            cols.join(",\n  ")
        ));
    }
    if n_tbl > 0 && rng.random_bool(0.6) {
        s.push_str("CREATE VIEW v_0 AS SELECT id, id + 1 AS idp, c0 FROM r_0;\n");
    }
    if rng.random_bool(0.5) {
        s.push_str("CREATE SCHEMA s1;\nCREATE TABLE s1.t (id BIGINT PRIMARY KEY, val TEXT);\n");
    }
    s
}
