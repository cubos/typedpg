//! The binding's API against PostgreSQL's own parser.

use typedpg_pg_query::NodeRef;
use typedpg_pg_query::ast::{self, Node};

#[test]
fn parses_with_postgres_18_grammar() {
    assert!(typedpg_pg_query::PG_VERSION.starts_with("18."));
    // RETURNING OLD / NEW is PostgreSQL 18 syntax.
    let parsed =
        typedpg_pg_query::parse("UPDATE t SET a = 1 RETURNING WITH (OLD AS o, NEW AS n) o.a, n.a")
            .unwrap();
    assert_eq!(parsed.tree.version, typedpg_pg_query::PG_VERSION_NUM);
    assert!(matches!(parsed.tree.nodes()[0].0, NodeRef::UpdateStmt(_)));
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
    let nodes = parsed.tree.nodes();
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
    assert_eq!(stmt.nodes().len(), nodes.len());
}

#[test]
fn nodes_mut_edits_the_tree_in_place() {
    let mut parsed = typedpg_pg_query::parse("SELECT a FROM t").unwrap();
    for (n, _) in unsafe { parsed.tree.nodes_mut() } {
        if let typedpg_pg_query::NodeMut::RangeVar(rv) = n {
            unsafe { (*rv).relname = "renamed".into() };
        }
    }
    let relations: Vec<String> = parsed
        .tree
        .nodes()
        .iter()
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
    let json = typedpg_pg_query::parse_plpgsql(
        "CREATE FUNCTION f() RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$",
    )
    .unwrap();
    assert!(json.to_string().contains("PLpgSQL_stmt_return"), "{json}");
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
    assert!(typedpg_pg_query::parse_plpgsql("CREATE FUNCTION f() RETURNS int RETURN 1").is_ok());
}

#[test]
fn plpgsql_trigger_functions_dump_valid_json() {
    // TG_* variables are promise datums; `RETURN NEW` is a retvarno.
    let json = typedpg_pg_query::parse_plpgsql(
        "CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$",
    )
    .unwrap();
    let text = json.to_string();
    assert!(text.contains("\"refname\":\"tg_name\""), "{text}");
    // `RETURN r` of a record variable: retvarno is r's datum number
    // (datums: found = 0, x = 1, r = 2). Zero values are omitted.
    let json = typedpg_pg_query::parse_plpgsql(
        "CREATE FUNCTION f() RETURNS record LANGUAGE plpgsql AS \
         $$ DECLARE x int; r record; BEGIN RETURN r; END $$",
    )
    .unwrap();
    assert!(json.to_string().contains("\"retvarno\":2"), "{json}");
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
    let json = typedpg_pg_query::parse_plpgsql(
        "CREATE FUNCTION f(a anycompatiblemultirange) RETURNS anycompatiblemultirange \
         LANGUAGE plpgsql AS $$ BEGIN RETURN a; END $$",
    )
    .unwrap();
    let datums = json[0]["PLpgSQL_function"]["datums"].as_array().unwrap();
    let type_of = |name: &str| {
        datums
            .iter()
            .map(|d| &d["PLpgSQL_var"])
            .find(|v| v["refname"] == name)
            .map(|v| v["datatype"]["PLpgSQL_type"]["typname"].clone())
    };
    assert_eq!(type_of("a"), Some("int4multirange".into()), "{json}");
    assert_eq!(type_of("$0"), Some("int4multirange".into()), "{json}");
}
