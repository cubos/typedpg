//! The binding's API against PostgreSQL's own parser.

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{self, Node};
use typedpg_pg_query::plpgsql::{self, Datum, Stmt};

/// The only function `sql` compiles to.
fn compile_one(sql: &str) -> plpgsql::Function {
    let mut functions = typedpg_pg_query::parse_plpgsql(sql).unwrap();
    assert_eq!(functions.len(), 1, "{sql}: {functions:?}");
    functions.remove(0)
}

/// The statements of a function's body.
fn body(function: &plpgsql::Function) -> &[Stmt] {
    &function.action.as_ref().expect("a body").body
}

#[test]
fn parses_with_postgres_19_grammar() {
    assert!(typedpg_pg_query::PG_VERSION.starts_with("19."));
    // ON CONFLICT DO SELECT is PostgreSQL 19 syntax.
    let parsed = typedpg_pg_query::parse(
        "INSERT INTO t VALUES (1) ON CONFLICT (a) DO SELECT FOR UPDATE RETURNING *",
    )
    .unwrap();
    assert_eq!(parsed.tree.version, typedpg_pg_query::PG_VERSION_NUM);
    assert!(matches!(
        parsed.tree.nodes().next(),
        Some((NodeRef::InsertStmt(_), 0))
    ));
    // So are IGNORE NULLS and REPACK.
    for sql in [
        "SELECT lag(a) IGNORE NULLS OVER (ORDER BY b) FROM t",
        "REPACK (CONCURRENTLY) t USING INDEX t_pkey",
    ] {
        typedpg_pg_query::parse(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
}

#[test]
fn comments_between_tokens_are_ignored() {
    // libpg_query 18.0 failed on a comment between tokens the grammar
    // looks at together (`NOT IN`, string constants concatenated).
    for sql in [
        "SELECT 1 WHERE 1 NOT /* not */ IN (2)",
        "SELECT 'a' -- the first half\n 'b'",
    ] {
        typedpg_pg_query::parse(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
}

#[test]
fn a_too_deep_query_is_an_error_not_a_crash() {
    // libpg_query 18.0 overflowed the stack (and aborted the process: the
    // compiler, under the sql! macro).
    // Not parentheses: the grammar's own stack limit ("memory exhausted")
    // stops those first. A UNION chain parses flat but nests deep.
    let sql = format!("SELECT 1{}", " UNION SELECT 1".repeat(100_000));
    let err = typedpg_pg_query::parse(&sql).unwrap_err();
    assert!(
        matches!(&err, typedpg_pg_query::Error::Parse { message, .. }
            if message.starts_with("stack depth limit exceeded")),
        "{err:?}"
    );
}

#[test]
fn the_deepest_query_parses_on_a_small_stack() {
    // 4,995 terms: libpg_query's 10,000 levels. Parsing and decoding it
    // takes about 10 MB of stack, grown as needed (src/stack.rs).
    let sql = format!("SELECT 1{}", " + 1".repeat(4995));
    let parsed = std::thread::Builder::new()
        .stack_size(256 << 10)
        .spawn(move || typedpg_pg_query::parse(&sql).map(|p| p.tree.stmts.len()))
        .unwrap()
        .join()
        .unwrap();
    assert_eq!(parsed.unwrap(), 1);
}

#[test]
fn the_deepest_tree_drops_on_a_small_stack() {
    // Dropping a tree recurses once per level; `NodeBox` grows the stack as
    // it goes, so the deepest tree drops on a 256 kB thread stack, in a
    // debug build too.
    let sql = format!("SELECT 1{}", " + 1".repeat(4995));
    let parsed = typedpg_pg_query::parse(&sql).unwrap();
    let dropped = std::thread::Builder::new()
        .stack_size(256 << 10)
        .spawn(move || {
            let copy = parsed.tree.clone();
            drop(parsed);
            copy.stmts.len()
        })
        .unwrap()
        .join()
        .unwrap();
    assert_eq!(dropped, 1);
}

#[test]
fn the_deepest_tree_is_walked_on_a_small_stack() {
    // The walkers queue nodes instead of recursing, so they reach every
    // level of the deepest tree on a 256 kB thread stack, in a debug build
    // too.
    let sql = format!("SELECT 1{}", " + 1".repeat(4995));
    let mut parsed = typedpg_pg_query::parse(&sql).unwrap();
    let depths = std::thread::Builder::new()
        .stack_size(256 << 10)
        .spawn(move || {
            let deepest = parsed.tree.nodes().map(|(_, d)| d).max().unwrap();
            let mut deepest_mut = 0;
            parsed
                .tree
                .for_each_node_mut(|_, d| deepest_mut = deepest_mut.max(d));
            (deepest, deepest_mut, parsed)
        })
        .unwrap()
        .join()
        .unwrap();
    assert!(depths.0 > 4995, "{}", depths.0);
    assert_eq!(depths.0, depths.1);
}

#[test]
fn parse_errors_carry_the_server_message() {
    let err = typedpg_pg_query::parse("SELECT FROM WHERE").unwrap_err();
    assert_eq!(
        err,
        typedpg_pg_query::Error::Parse {
            message: "syntax error at or near \"WHERE\"".into(),
            position: Some(12),
            funcname: Some("scanner_yyerror".into()),
        }
    );
    // The position is a byte offset, though PG counts characters.
    let err = typedpg_pg_query::parse("SELECT 'é' FROM WHERE").unwrap_err();
    assert!(
        matches!(
            err,
            typedpg_pg_query::Error::Parse {
                position: Some(17),
                ..
            }
        ),
        "{err:?}"
    );
    let err = typedpg_pg_query::parse("SELECT 1 +").unwrap_err();
    assert_eq!(
        err,
        typedpg_pg_query::Error::Parse {
            message: "syntax error at end of input".into(),
            position: Some(10),
            funcname: Some("scanner_yyerror".into()),
        }
    );
    // The raising function tells apart errors PG words alike: the scanner's
    // malformed `E''` escape from the parser's malformed `U&''` one.
    for (sql, func) in [
        (r"SELECT E'\u006'", "core_yylex"),
        (r"SELECT U&'\zz'", "str_udeescape"),
    ] {
        let err = typedpg_pg_query::parse(sql).unwrap_err();
        assert!(
            matches!(
                &err,
                typedpg_pg_query::Error::Parse { message, funcname, .. }
                    if message == "invalid Unicode escape" && funcname.as_deref() == Some(func)
            ),
            "{sql}: {err:?}"
        );
    }
    assert!(matches!(
        typedpg_pg_query::parse("SELECT '\0'"),
        Err(typedpg_pg_query::Error::Conversion(_))
    ));
}

#[test]
fn nodes_reaches_every_expression() {
    let parsed = typedpg_pg_query::parse(
        "SELECT a + (SELECT max(b) FROM u WHERE u.c = t.d) FROM t WHERE e IN (1, 2) ORDER BY f",
    )
    .unwrap();
    let nodes: Vec<_> = parsed.tree.nodes().collect();
    // The statement itself comes first, at depth 0.
    assert!(matches!(nodes[0], (NodeRef::SelectStmt(_), 0)));
    let mut columns: Vec<String> = nodes
        .iter()
        .filter_map(|(n, _)| match n {
            NodeRef::ColumnRef(cr) => match cr.fields.last() {
                Some(Some(Node::String(s))) => Some(s.sval.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    columns.sort();
    assert_eq!(columns, ["a", "b", "c", "d", "e", "f"]);
    // A subtree walk starts at its own root.
    let stmt = parsed.tree.stmts[0].stmt.as_ref().unwrap();
    assert_eq!(stmt.nodes().count(), nodes.len());
}

#[test]
fn nodes_is_lazy_and_visits_in_breadth_first_order() {
    let parsed = typedpg_pg_query::parse("SELECT a, b FROM t; SELECT 1").unwrap();
    let kinds: Vec<(String, usize)> = parsed
        .tree
        .nodes()
        .map(|(n, depth)| {
            let kind = format!("{n:?}");
            (kind[..kind.find('(').unwrap()].to_owned(), depth)
        })
        .collect();
    // Both statements first, then each level in field order.
    assert_eq!(
        &kinds[..2],
        [("SelectStmt".into(), 0), ("SelectStmt".into(), 0)]
    );
    assert!(kinds.windows(2).all(|w| w[0].1 <= w[1].1), "{kinds:?}");
    // The second level is the first statement's children.
    let first = parsed.tree.nodes().next().unwrap().0;
    let children: Vec<_> = first.children().map(|c| c.location()).collect();
    let second_level: Vec<_> = parsed
        .tree
        .nodes()
        .skip(2)
        .take(children.len())
        .map(|(n, _)| n.location())
        .collect();
    assert_eq!(children, second_level);
    assert!(first.children().any(|c| matches!(c, NodeRef::RangeVar(_))));
    assert!(!first.children().any(|c| matches!(c, NodeRef::ColumnRef(_))));
    // More children than the inline buffer holds spill, in order.
    let wide = typedpg_pg_query::parse("SELECT f(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11)").unwrap();
    let call = wide
        .tree
        .nodes()
        .find(|(n, _)| matches!(n, NodeRef::FuncCall(_)))
        .unwrap()
        .0;
    let args = call.children();
    assert_eq!(args.len(), 12);
    let locations: Vec<_> = args.filter_map(|c| c.location()).collect();
    assert!(
        locations.is_sorted() && locations.len() == 11,
        "{locations:?}"
    );
}

#[test]
fn for_each_node_mut_edits_the_tree_in_place() {
    let mut parsed = typedpg_pg_query::parse("SELECT a FROM t").unwrap();
    let order: Vec<_> = parsed
        .tree
        .nodes()
        .map(|(n, d)| (n.location(), d))
        .collect();
    let mut seen = Vec::new();
    parsed.tree.for_each_node_mut(|n, depth| {
        if let typedpg_pg_query::NodeMut::RangeVar(rv) = n {
            seen.push((Some(rv.location), depth));
            rv.relname = "renamed".into();
        } else {
            seen.push((None, depth));
        }
    });
    // The same nodes, in the same order, as `nodes()`.
    assert_eq!(seen.len(), order.len());
    assert!(
        seen.iter()
            .zip(&order)
            .all(|(s, o)| s.1 == o.1 && (s.0.is_none() || s.0 == o.0))
    );
    let relations: Vec<String> = parsed
        .tree
        .nodes()
        .filter_map(|(n, _)| match n {
            NodeRef::RangeVar(rv) => Some(rv.relname.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(relations, ["renamed"]);
}

#[test]
fn scans_tokens() {
    let scan = typedpg_pg_query::scan("SELECT $1 FROM t").unwrap();
    let kinds: Vec<ast::Token> = scan.tokens.iter().map(|t| t.token).collect();
    assert_eq!(
        kinds,
        [
            ast::Token::Select,
            ast::Token::Param,
            ast::Token::From,
            ast::Token::Ident
        ]
    );
}

#[test]
fn parses_plpgsql_bodies() {
    let function = compile_one(
        "CREATE FUNCTION f() RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$",
    );
    let [Stmt::Return(ret)] = body(&function) else {
        panic!("{function:?}")
    };
    assert_eq!(ret.expr.as_ref().map(|e| e.query.as_str()), Some("1"));
}

#[test]
fn plpgsql_functions_without_a_string_body_are_errors_not_aborts() {
    // Each of these used to fail a C assert in libpg_query and abort.
    for (sql, message) in [
        (
            "CREATE FUNCTION f(a int) RETURNS int LANGUAGE plpgsql RETURN a + 1",
            "inline SQL function body only valid for language SQL",
        ),
        (
            "CREATE PROCEDURE p() LANGUAGE plpgsql BEGIN ATOMIC SELECT 1; END",
            "inline SQL function body only valid for language SQL",
        ),
        (
            "CREATE FUNCTION f() RETURNS int LANGUAGE plpgsql",
            "no function body specified",
        ),
    ] {
        match typedpg_pg_query::parse_plpgsql(sql) {
            Err(typedpg_pg_query::Error::Parse { message: msg, .. }) => {
                assert_eq!(msg, message, "{sql}")
            }
            other => panic!("{sql}: {other:?}"),
        }
    }
    // Without LANGUAGE an inline body is SQL: nothing to compile.
    assert_eq!(
        typedpg_pg_query::parse_plpgsql("CREATE FUNCTION f() RETURNS int RETURN 1"),
        Ok(Vec::new())
    );
}

#[test]
fn plpgsql_trigger_functions_convert() {
    // TG_* variables are promise datums.
    let function = compile_one(
        "CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$",
    );
    assert!(
        function.datums.iter().any(|d| matches!(
            d,
            Datum::Promise(v) if v.refname == "tg_name" && v.promise == plpgsql::PromiseType::TgName
        )),
        "{function:?}"
    );
    // `RETURN r` of a record variable: retvarno is r's datum number
    // (datums: found = 0, x = 1, r = 2).
    let function = compile_one(
        "CREATE FUNCTION f() RETURNS record LANGUAGE plpgsql AS \
         $$ DECLARE x int; r record; BEGIN RETURN r; END $$",
    );
    let [Stmt::Return(ret)] = body(&function) else {
        panic!("{function:?}")
    };
    assert_eq!((ret.expr.as_ref(), ret.retvarno), (None, 2), "{function:?}");
    assert_eq!(function.datums[2].refname(), Some("r"));
}

#[test]
fn equal_compares_trees_like_postgres_equal() {
    use typedpg_pg_query::Equal;
    let expr = |sql: &str| {
        let parsed = typedpg_pg_query::parse(&format!("SELECT {sql}")).unwrap();
        let Some(Node::SelectStmt(sel)) = parsed.tree.stmts[0].stmt.clone() else {
            unreachable!()
        };
        sel.target_list[0].clone()
    };
    // Layout and case of keywords don't matter: locations aren't compared.
    assert!(expr("a + 1 > 0").equal(&expr("a+1>0")));
    assert!(expr("x IN (1, 2)").equal(&expr("x in (1,2)")));
    assert!(expr("CASE WHEN a THEN 1 END").equal(&expr("case  when a then 1 end")));
    // Anything else does.
    assert!(!expr("a + 1 > 0").equal(&expr("a + 2 > 0")));
    assert!(!expr("a > 0").equal(&expr("b > 0")));
    assert!(!expr("(a > 0)::int").equal(&expr("(a > 0)::bigint")));
    assert!(!expr("x IN (1, 2)").equal(&expr("x IN (2, 1)")));
}

#[test]
fn rewrite_replaces_nodes_bottom_up() {
    use typedpg_pg_query::Equal;
    // Replace every `x::int` with `x`, below a node `descend` refuses
    // excepted: the nested cast goes first, so both layers go.
    let parsed =
        typedpg_pg_query::parse("SELECT a::int::int + 1, (SELECT b::int), c::text").unwrap();
    let mut stmt = parsed.tree.stmts[0].stmt.clone().unwrap();
    stmt.rewrite(&mut |n| !matches!(n, Node::SubLink(_)), &mut |n| {
        if let Node::TypeCast(tc) = n
            && tc.type_name.as_ref().is_some_and(|tn| {
                tn.names
                    .iter()
                    .any(|s| matches!(s, Some(Node::String(s)) if s.sval == "int4"))
            })
            && let Some(arg) = tc.arg.take()
        {
            *n = arg;
        }
    });
    // The tree now equals the one of the rewritten SQL: `a + 1` (both casts
    // gone), the subquery untouched, and `c::text` intact.
    let Node::SelectStmt(rewritten) = stmt else {
        unreachable!()
    };
    let expected = typedpg_pg_query::parse("SELECT a + 1, (SELECT b::int), c::text").unwrap();
    let Some(Node::SelectStmt(expected)) = expected.tree.stmts[0].stmt.clone() else {
        unreachable!()
    };
    assert!(rewritten.target_list.equal(&expected.target_list));
}

#[test]
fn plpgsql_validates_anycompatiblemultirange_as_postgres_18_7() {
    // Validation resolves polymorphic types to int4 ones: since PostgreSQL
    // 18.7 (patches.rs), anycompatiblemultirange to int4multirange, as
    // anymultirange, for the arguments and the result ($0) alike.
    let function = compile_one(
        "CREATE FUNCTION f(a anycompatiblemultirange) RETURNS anycompatiblemultirange \
         LANGUAGE plpgsql AS $$ BEGIN RETURN a; END $$",
    );
    let type_of = |name: &str| {
        function.datums.iter().find_map(|d| match d {
            Datum::Var(v) if v.refname == name => v.datatype.as_ref().map(|t| t.typname.as_str()),
            _ => None,
        })
    };
    assert_eq!(type_of("a"), Some("int4multirange"), "{function:?}");
    assert_eq!(type_of("$0"), Some("int4multirange"), "{function:?}");
}

#[test]
fn the_deepest_plpgsql_converts_and_drops_on_a_small_stack() {
    // PL/pgSQL's grammar nests 3,330 blocks (3,329 IFs) in a DO
    // block's own, and no more: its parser stack (bison's YYMAXDEPTH, 10,000
    // entries) runs out. The deepest tree converts, clones and drops on a
    // 256 kB thread stack, in a debug build too.
    let deepest = |open: &str, close: &str, n: usize| {
        format!("DO $$ BEGIN {}{} END $$", open.repeat(n), close.repeat(n))
    };
    for (open, close, max) in [
        ("BEGIN ", "END; ", 3330),
        ("IF true THEN ", "END IF; ", 3329),
    ] {
        let too_deep = typedpg_pg_query::parse_plpgsql(&deepest(open, close, max + 1));
        assert!(
            matches!(&too_deep, Err(typedpg_pg_query::Error::Parse { message, .. })
                if message.starts_with("memory exhausted")),
            "{too_deep:?}"
        );
        let sql = deepest(open, close, max);
        let depth = std::thread::Builder::new()
            .stack_size(256 << 10)
            .spawn(move || {
                let functions = typedpg_pg_query::parse_plpgsql(&sql).unwrap();
                let copy = functions.clone();
                drop(functions);
                // Down the first statement of each body to the innermost
                // (the outermost body also holds the dummy RETURN the
                // compiler appends).
                let mut depth = 0;
                let mut level = copy[0].action.as_ref().map(|b| b.body.as_slice());
                while let Some([stmt, ..]) = level {
                    depth += 1;
                    level = match stmt {
                        Stmt::Block(b) => Some(b.body.as_slice()),
                        Stmt::If(i) => Some(i.then_body.as_slice()),
                        _ => None,
                    };
                }
                depth
            })
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(depth, max, "{open}");
    }
}

#[test]
fn option_dump_prints_nothing() {
    // `#option dump` asks PL/pgSQL's compiler to print the function's tree
    // to stdout (plpgsql_dumptree) — the stdout of whatever runs the
    // analyzer. The option is accepted and prints nothing. Checked in a
    // child process: the test harness captures only what Rust prints.
    const CHILD: &str = "TYPEDPG_OPTION_DUMP_CHILD";
    if std::env::var_os(CHILD).is_some() {
        for sql in [
            "CREATE FUNCTION f() RETURNS int LANGUAGE plpgsql AS $$\n#option dump\nBEGIN RETURN 1; END $$",
            "DO $$\n#option dump\nBEGIN NULL; END $$",
        ] {
            compile_one(sql);
        }
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "option_dump_prints_nothing", "--nocapture"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("1 passed"), "{stdout}");
    assert!(!stdout.contains("Execution tree"), "{stdout}");
}
