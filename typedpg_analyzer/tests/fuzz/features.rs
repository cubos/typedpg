//! Feature generator: statements over the SQL surface the template generator
//! never reaches — row-value comparisons, set-returning functions in FROM
//! (`WITH ORDINALITY`, `ROWS FROM`), SQL/JSON functions and `JSON_TABLE`,
//! datetime syntax (`OVERLAPS`, `AT TIME ZONE`, `EXTRACT`), quantified
//! comparisons, system columns, recursive CTEs with `SEARCH`/`CYCLE`,
//! `RETURNING old/new` and `merge_action()`, multi-branch set-operation type
//! resolution, window frames with offsets and `EXCLUDE`, range and multirange
//! operators, domains over arrays and composites, generated columns, and
//! inheritance and partitioned tables (the feature part of the base
//! schema), and schema-qualified references (`schema.table.column`) against
//! aliased, renamed and hidden FROM entries.
//!
//! Kept apart from `gen_statement` (the printer's round-trip test holds that
//! one to a printable share; the SQL/JSON nodes here have no printer).

use super::*;

/// One statement from the feature generator.
pub(crate) fn gen_feature_statement(rng: &mut StdRng) -> String {
    let np = &mut 0u32;
    match rng.random_range(0..16) {
        0 => gen_row_comparison(rng, np),
        1 => gen_srf_from(rng, np),
        2 => gen_sql_json(rng, np),
        3 => gen_json_table(rng, np),
        4 => gen_datetime(rng, np),
        5 => gen_quantified(rng, np),
        6 => gen_system_columns(rng),
        7 => gen_recursive_cte(rng, np),
        8 => gen_returning_old_new(rng, np),
        9 => gen_union_chain(rng, np),
        10 => gen_window_frame(rng, np),
        11 => gen_range_ops(rng, np),
        12 => gen_domain_composite(rng, np),
        13 => gen_inheritance(rng, np),
        14 => gen_qualified_ref(rng),
        _ => gen_grouping(rng, np),
    }
}

fn pick<'a>(rng: &mut StdRng, items: &[&'a str]) -> &'a str {
    items[rng.random_range(0..items.len())]
}

fn param(np: &mut u32) -> String {
    format!("${}", next_param(np))
}

/// A value for a row-comparison slot: a literal, a bare parameter, or a
/// deliberately mistyped literal.
fn row_slot(ty: Ty, rng: &mut StdRng, np: &mut u32) -> String {
    match rng.random_range(0..10) {
        0..=5 => literal_for(ty, rng),
        6..=8 => param(np),
        _ => scalar_literal(rng),
    }
}

fn gen_row_comparison(rng: &mut StdRng, np: &mut u32) -> String {
    let table = &TABLES[rng.random_range(0..TABLES.len())];
    let a = random_col(table, rng);
    let b = random_col(table, rng);
    let pred = match rng.random_range(0..7) {
        0 => format!(
            "({}, {}) {} ({}, {})",
            a.name,
            b.name,
            pick(rng, &["=", "<>", "<", "<=", ">", ">="]),
            row_slot(a.ty, rng, np),
            row_slot(b.ty, rng, np)
        ),
        1 => format!(
            "ROW({}, {}) IS {}NULL",
            a.name,
            b.name,
            pick(rng, &["", "NOT "])
        ),
        2 => format!(
            "({}, {}) IS {}DISTINCT FROM ({}, {})",
            a.name,
            b.name,
            pick(rng, &["", "NOT "]),
            row_slot(a.ty, rng, np),
            row_slot(b.ty, rng, np)
        ),
        3 => format!(
            "({}, {}) {}IN (SELECT {}, {} FROM {})",
            a.name,
            b.name,
            pick(rng, &["", "NOT "]),
            a.name,
            b.name,
            table.name
        ),
        4 => format!(
            "({}, {}) {} ANY (SELECT {}, {} FROM {})",
            a.name,
            b.name,
            pick(rng, &["=", "<", "<>"]),
            a.name,
            b.name,
            table.name
        ),
        5 => format!("NOT (ROW({}, {}) IS NULL)", a.name, b.name),
        _ => format!(
            "({}, {}) IN (({}, {}), ({}, {}))",
            a.name,
            b.name,
            row_slot(a.ty, rng, np),
            row_slot(b.ty, rng, np),
            row_slot(a.ty, rng, np),
            row_slot(b.ty, rng, np)
        ),
    };
    let proj = if rng.random_bool(0.3) {
        format!("{pred} AS c0, {}", a.name)
    } else {
        format!("{}, {}", a.name, b.name)
    };
    format!("SELECT {proj} FROM {} WHERE {pred}", table.name)
}

fn gen_srf_from(rng: &mut StdRng, np: &mut u32) -> String {
    let ord = if rng.random_bool(0.5) {
        " WITH ORDINALITY"
    } else {
        ""
    };
    let lateral = rng.random_bool(0.5);
    let srf = match rng.random_range(0..9) {
        0 => format!(
            "generate_series({}, {})",
            lit_or_param(Ty::Int, rng, np),
            lit_or_param(Ty::Int, rng, np)
        ),
        1 => "generate_series(now(), now() + interval '2 days', interval '1 day')".to_string(),
        2 if lateral => "unnest(users.nums)".to_string(),
        2 => "unnest(ARRAY[1, NULL, 3])".to_string(),
        3 if lateral => "unnest(users.tags, users.nums)".to_string(),
        3 => format!("unnest({}::int[], ARRAY['a'])", param(np)),
        4 if lateral => "jsonb_array_elements(users.prefs)".to_string(),
        4 => "jsonb_each('{\"a\": 1, \"b\": null}'::jsonb)".to_string(),
        5 if lateral => "jsonb_each_text(users.prefs)".to_string(),
        5 => "jsonb_to_recordset('[{\"a\": 1}]') AS x(a int, b text)".to_string(),
        6 => "regexp_matches('a1b2', '([a-z])([0-9])', 'g')".to_string(),
        7 if lateral => "string_to_table(users.name, ',')".to_string(),
        7 => "ROWS FROM (generate_series(1, 3), unnest(ARRAY['x', 'y']))".to_string(),
        _ => "generate_subscripts(ARRAY[[1, 2], [3, 4]], 2)".to_string(),
    };
    let has_coldef = srf.contains(" AS x(");
    let srf = if has_coldef {
        // A column definition list takes no WITH ORDINALITY.
        srf
    } else {
        format!("{srf}{ord} AS f")
    };
    let proj = pick(rng, &["*", "f.*", "f"]);
    let proj = if has_coldef && proj != "*" {
        "x.*"
    } else {
        proj
    };
    if lateral {
        let join = pick(
            rng,
            &[", LATERAL", " CROSS JOIN LATERAL", " LEFT JOIN LATERAL"],
        );
        let on = if join.contains("LEFT") {
            " ON true"
        } else {
            ""
        };
        format!("SELECT users.id, {proj} FROM users{join} {srf}{on}")
    } else {
        format!("SELECT {proj} FROM {srf}")
    }
}

fn json_returning(rng: &mut StdRng) -> String {
    match rng.random_range(0..6) {
        0 => " RETURNING int".to_string(),
        1 => " RETURNING text".to_string(),
        2 => " RETURNING jsonb".to_string(),
        3 => " RETURNING numeric(5, 1)".to_string(),
        4 => " RETURNING date".to_string(),
        _ => String::new(),
    }
}

fn gen_sql_json(rng: &mut StdRng, np: &mut u32) -> String {
    let path = pick(
        rng,
        &["'$.a'", "'$.a[*]'", "'$'", "'strict $.b'", "'lax $.c.d'"],
    );
    let behavior = |rng: &mut StdRng, what: &str| -> String {
        match rng.random_range(0..5) {
            0 => format!(" NULL ON {what}"),
            1 => format!(" ERROR ON {what}"),
            2 => format!(" DEFAULT 0 ON {what}"),
            3 => format!(" DEFAULT 'x' ON {what}"),
            _ => String::new(),
        }
    };
    let expr = match rng.random_range(0..10) {
        0 => format!(
            "JSON_VALUE(prefs, {path}{}{}{})",
            json_returning(rng),
            behavior(rng, "EMPTY"),
            behavior(rng, "ERROR")
        ),
        1 => format!(
            "JSON_QUERY(prefs, {path}{}{}{})",
            json_returning(rng),
            pick(
                rng,
                &[
                    "",
                    " WITH WRAPPER",
                    " WITH CONDITIONAL WRAPPER",
                    " WITHOUT WRAPPER"
                ]
            ),
            pick(rng, &["", " OMIT QUOTES", " KEEP QUOTES"])
        ),
        2 => format!(
            "JSON_EXISTS(prefs, {path}{})",
            pick(
                rng,
                &["", " TRUE ON ERROR", " UNKNOWN ON ERROR", " ERROR ON ERROR"]
            )
        ),
        3 => format!(
            "JSON_OBJECT('n': name, 'a': age{}{})",
            pick(rng, &["", " NULL ON NULL", " ABSENT ON NULL"]),
            json_returning(rng)
        ),
        4 => format!(
            "JSON_ARRAY(name, age, {}{})",
            lit_or_param(Ty::Int, rng, np),
            pick(rng, &["", " NULL ON NULL", " ABSENT ON NULL"])
        ),
        5 => format!(
            "{} IS {}JSON{}{}",
            pick(rng, &["name", "prefs", "prefs::text", "age"]),
            pick(rng, &["", "NOT "]),
            pick(rng, &["", " VALUE", " OBJECT", " ARRAY", " SCALAR"]),
            pick(rng, &["", " WITH UNIQUE KEYS", " WITHOUT UNIQUE KEYS"])
        ),
        6 => format!(
            "JSON({}{})",
            pick(rng, &["name", "'{\"a\": 1}'", "prefs::text", "age"]),
            pick(rng, &["", " WITH UNIQUE KEYS"])
        ),
        7 => format!(
            "JSON_SCALAR({})",
            pick(
                rng,
                &["age", "name", "score", "created_at", "active", "NULL"]
            )
        ),
        8 => format!(
            "JSON_SERIALIZE(prefs{})",
            pick(
                rng,
                &["", " RETURNING text", " RETURNING bytea", " RETURNING int"]
            )
        ),
        _ => {
            return format!(
                "SELECT {} FROM users GROUP BY st",
                pick(
                    rng,
                    &[
                        "JSON_ARRAYAGG(age)",
                        "JSON_ARRAYAGG(age ORDER BY id ABSENT ON NULL)",
                        "JSON_ARRAYAGG(name NULL ON NULL RETURNING jsonb)",
                        "JSON_OBJECTAGG(name: age)",
                        "JSON_OBJECTAGG(name VALUE age ABSENT ON NULL WITH UNIQUE KEYS)",
                        "JSON_OBJECTAGG(name: age RETURNING jsonb)",
                    ]
                )
            );
        }
    };
    if rng.random_bool(0.3) {
        format!("SELECT id FROM users WHERE ({expr}) IS NOT NULL")
    } else {
        format!("SELECT {expr} AS c0 FROM users")
    }
}

fn gen_json_table(rng: &mut StdRng, np: &mut u32) -> String {
    let mut cols = vec!["n FOR ORDINALITY".to_string()];
    for i in 0..rng.random_range(1..4) {
        let ty = pick(rng, &["int", "text", "numeric", "bool", "jsonb", "date"]);
        let col = match rng.random_range(0..5) {
            0 => format!("c{i} {ty} PATH '$.k{i}'"),
            1 => format!(
                "c{i} {ty} PATH '$.k{i}' DEFAULT {} ON EMPTY",
                lit_or_param(Ty::Int, rng, np)
            ),
            2 => format!("c{i} bool EXISTS PATH '$.k{i}'"),
            3 => format!("c{i} jsonb FORMAT JSON PATH '$.k{i}' WITH WRAPPER"),
            _ => format!("c{i} {ty} PATH '$.k{i}' ERROR ON ERROR"),
        };
        cols.push(col);
    }
    if rng.random_bool(0.3) {
        cols.push("NESTED PATH '$.items[*]' COLUMNS (item int PATH '$')".to_string());
    }
    let src = if rng.random_bool(0.6) {
        "users.prefs"
    } else {
        "'[{\"k0\": 1}, {\"k1\": \"x\"}]'::jsonb"
    };
    let tail = pick(rng, &["", " ERROR ON ERROR", " EMPTY ON ERROR"]);
    let from = if src.starts_with("users") {
        "users, "
    } else {
        ""
    };
    format!(
        "SELECT jt.* FROM {from}JSON_TABLE({src}, '$[*]' COLUMNS ({}){tail}) AS jt",
        cols.join(", ")
    )
}

fn gen_datetime(rng: &mut StdRng, np: &mut u32) -> String {
    let expr = match rng.random_range(0..7) {
        0 => format!(
            "(created_at, {}) OVERLAPS ({}, {})",
            pick(
                rng,
                &["interval '1 day'", "now()", "NULL::timestamptz", "bday"]
            ),
            pick(rng, &["now()", "bday", "created_at"]),
            pick(rng, &["now()", "interval '2 hours'", "NULL::interval"])
        ),
        1 => format!(
            "{} AT TIME ZONE {}",
            pick(
                rng,
                &["created_at", "bday", "now()", "created_at::timestamp"]
            ),
            pick(rng, &["'UTC'", "'America/Sao_Paulo'", "interval '3 hours'"])
        ),
        2 => format!(
            "{} AT LOCAL",
            pick(rng, &["created_at", "created_at::timestamp"])
        ),
        3 => format!(
            "EXTRACT({} FROM {})",
            pick(rng, &["year", "epoch", "dow", "hour", "timezone", "julian"]),
            pick(
                rng,
                &["created_at", "bday", "interval '1 day'", "created_at::time"]
            )
        ),
        4 => format!(
            "date_trunc({}, {})",
            pick(rng, &["'day'", "'month'"]),
            pick(rng, &["created_at", "bday", "interval '1 day'"])
        ),
        5 => format!(
            "{} {} {}",
            pick(rng, &["created_at", "bday", "now()"]),
            pick(rng, &["+", "-"]),
            pick(
                rng,
                &["interval '1 day'", "1", "bday", "created_at", &param(np)]
            )
        ),
        _ => format!(
            "make_interval(days => {}) + {}",
            lit_or_param(Ty::Int, rng, np),
            pick(rng, &["bday", "created_at", "interval '1 hour'"])
        ),
    };
    format!("SELECT {expr} AS c0 FROM users")
}

fn gen_quantified(rng: &mut StdRng, np: &mut u32) -> String {
    let pred = match rng.random_range(0..6) {
        0 => format!("age {} ALL (nums)", pick(rng, &["=", "<>", "<", ">="])),
        1 => format!(
            "age {} ALL (SELECT views FROM posts)",
            pick(rng, &["=", ">"])
        ),
        2 => format!(
            "name LIKE {} (ARRAY['a%', {}])",
            pick(rng, &["ANY", "ALL"]),
            lit_or_param(Ty::Text, rng, np)
        ),
        3 => format!("{} = ANY (tags)", lit_or_param(Ty::Text, rng, np)),
        4 => format!("age = ALL ({})", param(np)),
        _ => format!(
            "age {} SOME (SELECT views FROM posts WHERE posts.user_id = users.id)",
            pick(rng, &["<", "="])
        ),
    };
    if rng.random_bool(0.5) {
        format!("SELECT {pred} AS c0 FROM users")
    } else {
        format!("SELECT id, age FROM users WHERE {pred}")
    }
}

fn gen_system_columns(rng: &mut StdRng) -> String {
    let table = pick(rng, &["users", "posts", "logs", "base_items", "events"]);
    let cols = pick(
        rng,
        &[
            "tableoid",
            "tableoid::regclass",
            "ctid",
            "xmin, xmax",
            "cmin, cmax",
            "ctid, id",
            "tableoid::regclass AS rel, count(*)",
        ],
    );
    let group = if cols.contains("count") {
        " GROUP BY 1"
    } else {
        ""
    };
    format!("SELECT {cols} FROM {table}{group}")
}

fn gen_recursive_cte(rng: &mut StdRng, np: &mut u32) -> String {
    let body = match rng.random_range(0..3) {
        0 => format!(
            "SELECT id, user_id, 1 AS depth FROM posts WHERE id = {} \
             UNION ALL SELECT p.id, p.user_id, r.depth + 1 FROM posts p JOIN r ON p.user_id = r.id",
            lit_or_param(Ty::BigInt, rng, np)
        ),
        1 => "SELECT 1::bigint AS id, NULL::bigint AS user_id, 0 AS depth \
              UNION SELECT id + 1, id, depth + 1 FROM r WHERE id < 5"
            .to_string(),
        _ => "SELECT id, user_id, 0 AS depth FROM posts UNION ALL \
              SELECT r.id, r.user_id, r.depth + 1 FROM r WHERE r.depth < 2"
            .to_string(),
    };
    let decor = match rng.random_range(0..4) {
        0 => " SEARCH DEPTH FIRST BY id SET ord",
        1 => " SEARCH BREADTH FIRST BY id, user_id SET ord",
        2 => " CYCLE id SET is_cycle USING path",
        _ => "",
    };
    let mat = pick(rng, &["", "MATERIALIZED ", "NOT MATERIALIZED "]);
    let proj = pick(rng, &["*", "id, depth", "max(depth)", "user_id"]);
    format!("WITH RECURSIVE r AS {mat}({body}){decor} SELECT {proj} FROM r")
}

fn gen_returning_old_new(rng: &mut StdRng, np: &mut u32) -> String {
    let ret = pick(
        rng,
        &[
            "old.*, new.*",
            "old.title, new.title",
            "old.body IS NULL, new.views",
            "old, new",
            "merge_action(), old.id, new.id",
            "*",
        ],
    );
    let with = if rng.random_bool(0.2) {
        "WITH (OLD AS o, NEW AS n) "
    } else {
        ""
    };
    let ret = if with.is_empty() {
        ret.to_string()
    } else {
        ret.replace("old", "o").replace("new", "n")
    };
    match rng.random_range(0..4) {
        0 => format!(
            "UPDATE posts SET views = views + 1, body = {} WHERE id = {} RETURNING {with}{}",
            lit_or_param(Ty::Text, rng, np),
            lit_or_param(Ty::BigInt, rng, np),
            ret.replace("merge_action(), ", "")
        ),
        1 => format!(
            "DELETE FROM posts WHERE views > {} RETURNING {with}{}",
            lit_or_param(Ty::Int, rng, np),
            ret.replace("merge_action(), ", "")
        ),
        2 => format!(
            "INSERT INTO posts (user_id, title) VALUES ({}, 'x') \
             ON CONFLICT (id) DO UPDATE SET title = excluded.title RETURNING {with}{}",
            lit_or_param(Ty::BigInt, rng, np),
            ret.replace("merge_action(), ", "")
        ),
        _ => format!(
            "MERGE INTO posts p USING users u ON p.user_id = u.id \
             WHEN MATCHED AND u.age > {} THEN DELETE \
             WHEN MATCHED THEN UPDATE SET title = u.name \
             WHEN NOT MATCHED THEN INSERT (user_id, title) VALUES (u.id, u.name) \
             RETURNING {with}{}",
            lit_or_param(Ty::Int, rng, np),
            ret
        ),
    }
}

fn gen_union_chain(rng: &mut StdRng, np: &mut u32) -> String {
    let n = rng.random_range(2..5);
    let ops = ["UNION", "UNION ALL", "INTERSECT", "EXCEPT"];
    let mut sql = String::new();
    for i in 0..n {
        if i > 0 {
            sql.push_str(&format!(" {} ", ops[rng.random_range(0..ops.len())]));
        }
        let item = match rng.random_range(0..9) {
            0 => "NULL".to_string(),
            1 => "1".to_string(),
            2 => "2.5".to_string(),
            3 => "'3'".to_string(),
            4 => param(np),
            5 => "age".to_string(),
            6 => "score".to_string(),
            7 => "id".to_string(),
            _ => "ratio".to_string(),
        };
        let from = if ["age", "score", "id", "ratio"].contains(&item.as_str()) {
            " FROM users"
        } else {
            ""
        };
        if rng.random_bool(0.2) {
            sql.push_str(&format!("(SELECT {item}{from})"));
        } else {
            sql.push_str(&format!("SELECT {item}{from}"));
        }
    }
    if rng.random_bool(0.3) {
        format!("SELECT * FROM ({sql}) AS s")
    } else {
        sql
    }
}

fn gen_window_frame(rng: &mut StdRng, np: &mut u32) -> String {
    let mode = pick(rng, &["ROWS", "RANGE", "GROUPS"]);
    let bound = |rng: &mut StdRng, np: &mut u32| -> String {
        match rng.random_range(0..6) {
            0 => "UNBOUNDED PRECEDING".to_string(),
            1 => "CURRENT ROW".to_string(),
            2 => format!("{} PRECEDING", rng.random_range(0..3)),
            3 => format!("{} FOLLOWING", rng.random_range(0..3)),
            4 => format!("{} PRECEDING", param(np)),
            _ => "UNBOUNDED FOLLOWING".to_string(),
        }
    };
    let start = bound(rng, np);
    let end = bound(rng, np);
    let exclude = pick(
        rng,
        &[
            "",
            "",
            " EXCLUDE CURRENT ROW",
            " EXCLUDE GROUP",
            " EXCLUDE TIES",
            " EXCLUDE NO OTHERS",
        ],
    );
    let order = pick(rng, &["id", "age", "created_at", "score", "id, age"]);
    let func = pick(
        rng,
        &[
            "sum(age)",
            "count(*)",
            "count(age)",
            "max(id)",
            "first_value(id)",
            "last_value(name)",
            "nth_value(id, 2)",
            "array_agg(id)",
            "lag(id)",
            "row_number()",
            "avg(score)",
        ],
    );
    let partition = if rng.random_bool(0.3) {
        "PARTITION BY st "
    } else {
        ""
    };
    format!(
        "SELECT id, {func} OVER ({partition}ORDER BY {order} {mode} BETWEEN {start} AND {end}{exclude}) AS w FROM users"
    )
}

fn gen_range_ops(rng: &mut StdRng, np: &mut u32) -> String {
    let lhs = pick(
        rng,
        &[
            "during",
            "slots",
            "span",
            "during * during",
            "slots + slots",
        ],
    );
    let op = pick(
        rng,
        &[
            "@>", "<@", "&&", "<<", ">>", "-|-", "&<", "&>", "*", "+", "-", "=",
        ],
    );
    let rhs = match rng.random_range(0..7) {
        0 => "now()".to_string(),
        1 => "tstzrange(now(), NULL)".to_string(),
        2 => "'{[1,3), [5,7)}'::int4multirange".to_string(),
        3 => "current_date".to_string(),
        4 => param(np),
        5 => "'empty'::daterange".to_string(),
        _ => "4".to_string(),
    };
    let expr = match rng.random_range(0..4) {
        0 => format!("{lhs} {op} {rhs}"),
        1 => format!(
            "{}({lhs})",
            pick(
                rng,
                &[
                    "lower",
                    "upper",
                    "isempty",
                    "lower_inf",
                    "upper_inc",
                    "range_merge",
                    "multirange"
                ]
            )
        ),
        2 => format!("range_agg({lhs})"),
        _ => format!("unnest({lhs})"),
    };
    let group = if expr.starts_with("range_agg") {
        " GROUP BY kind"
    } else {
        ""
    };
    format!("SELECT {expr} AS c0 FROM events{group}")
}

fn gen_domain_composite(rng: &mut StdRng, np: &mut u32) -> String {
    match rng.random_range(0..7) {
        0 => format!(
            "SELECT {} FROM events",
            pick(
                rng,
                &[
                    "labels[1]",
                    "labels || 'x'",
                    "array_length(labels, 1)",
                    "(pair).amount",
                    "(ppair).cur",
                    "(pair).*",
                    "pair",
                    "ppair",
                    "n",
                    "(ppair).amount + 1",
                    "labels",
                ]
            )
        ),
        1 => format!(
            "INSERT INTO events (id, span, labels, pair, ppair, kind) VALUES ({}, {}, {}, {}, {}, {}) RETURNING *",
            lit_or_param(Ty::BigInt, rng, np),
            pick(
                rng,
                &[
                    "'[2024-01-01,2024-02-01)'",
                    "daterange(current_date, NULL)",
                    "NULL"
                ]
            ),
            pick(rng, &["ARRAY['a']", "'{a,NULL}'", "NULL", "'{}'"]),
            pick(rng, &["ROW(1, 'usd')", "ROW(1)", "NULL", "'(2,eur)'"]),
            pick(
                rng,
                &["ROW(1, 'usd')", "ROW(-1, 'usd')", "NULL", "ROW(NULL, 'x')"]
            ),
            lit_or_param(Ty::Text, rng, np)
        ),
        2 => format!(
            "UPDATE events SET {} WHERE id = {} RETURNING n, pair, ppair",
            pick(
                rng,
                &[
                    "pair.amount = 5",
                    "ppair.amount = -1",
                    "labels[2] = 'x'",
                    "labels[1] = NULL",
                    "n = 1",
                    "kind = NULL",
                    "pair = ROW(1, 'x')",
                    "(pair.amount, pair.cur) = (1, 'x')",
                ]
            ),
            lit_or_param(Ty::BigInt, rng, np)
        ),
        3 => format!(
            "SELECT id FROM events WHERE {} = {}",
            pick(
                rng,
                &["labels", "pair", "ppair", "(pair).amount", "labels[1]"]
            ),
            param(np)
        ),
        4 => "SELECT (e.pair).amount, (e).ppair FROM events e".to_string(),
        5 => format!(
            "SELECT {}::{} AS c0",
            pick(rng, &["'{a,b}'", "ROW(1, 'x')", "'(1,x)'", "'{NULL}'"]),
            pick(rng, &["tag_list", "money_pair", "pos_pair", "text[]"])
        ),
        _ => "SELECT ppair, pair FROM events WHERE ppair IS NOT NULL AND (pair).cur IS NOT NULL"
            .to_string(),
    }
}

fn gen_inheritance(rng: &mut StdRng, np: &mut u32) -> String {
    match rng.random_range(0..7) {
        0 => format!(
            "SELECT {} FROM {}",
            pick(rng, &["*", "id, label", "count(*)"]),
            pick(
                rng,
                &[
                    "base_items",
                    "ONLY base_items",
                    "special_items",
                    "base_items*"
                ]
            )
        ),
        1 => format!(
            "SELECT * FROM {} WHERE at > {}",
            pick(rng, &["logs", "logs_2024", "ONLY logs", "logs_default"]),
            lit_or_param(Ty::Timestamptz, rng, np)
        ),
        2 => format!(
            "INSERT INTO {} (id, msg, at) VALUES ({}, 'm', {}) RETURNING tableoid::regclass, *",
            pick(rng, &["logs", "logs_2024", "logs_default"]),
            lit_or_param(Ty::BigInt, rng, np),
            pick(rng, &["'2024-06-01'", "'2030-01-01'", "now()", "NULL"])
        ),
        3 => format!(
            "UPDATE {} SET label = {} RETURNING *",
            pick(rng, &["base_items", "ONLY base_items", "special_items"]),
            lit_or_param(Ty::Text, rng, np)
        ),
        4 => "DELETE FROM logs WHERE msg IS NULL RETURNING id, at".to_string(),
        5 => "SELECT b.id, s.extra FROM base_items b LEFT JOIN special_items s ON s.id = b.id"
            .to_string(),
        _ => format!(
            "INSERT INTO special_items (id, label, extra) VALUES ({}, NULL, {}) RETURNING *",
            lit_or_param(Ty::BigInt, rng, np),
            pick(rng, &["1", "NULL", "'x'"])
        ),
    }
}

fn gen_grouping(rng: &mut StdRng, _np: &mut u32) -> String {
    let group = pick(
        rng,
        &[
            "GROUP BY DISTINCT ROLLUP (st, active), CUBE (st)",
            "GROUP BY ALL ROLLUP (st, active)",
            "GROUP BY GROUPING SETS ((st), (active), (st, active), ())",
            "GROUP BY st, ROLLUP (active)",
            "GROUP BY CUBE (st, active)",
            "GROUP BY ()",
        ],
    );
    let proj = pick(
        rng,
        &[
            "st, active, GROUPING(st, active), count(*)",
            "st, max(age), array_agg(id)",
            "GROUPING(st), sum(score)",
            "active, min(name)",
            "count(*)",
        ],
    );
    let having = pick(
        rng,
        &[
            "",
            " HAVING GROUPING(st) = 0",
            " HAVING st IS NOT NULL",
            " HAVING count(*) > 1",
        ],
    );
    format!("SELECT {proj} FROM users {group}{having}")
}

/// `schema.table.column` / `schema.table.*` against FROM entries that may
/// or may not be that relation under its own name.
fn gen_qualified_ref(rng: &mut StdRng) -> String {
    let from = pick(
        rng,
        &[
            "users",
            "users u",
            "public.users",
            "users users",
            "(users JOIN posts ON true) j",
            "users JOIN posts ON posts.user_id = users.id",
            "posts, LATERAL (SELECT public.posts.id AS pid) l",
            "users, (SELECT 1 AS one) s",
        ],
    );
    let refs = pick(
        rng,
        &[
            "public.users.id",
            "x.users.id",
            "public.users.nosuch",
            "public.users.*",
            "row_to_json(public.users.*)",
            "public.u.id",
            "pg_catalog.users.name",
            "public.posts.title",
            "users.users.id",
            "public.nosuch.id",
        ],
    );
    format!("SELECT {refs} FROM {from}")
}
