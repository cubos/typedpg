//! The binding's API against the vendored libpg_query.

use typedpg_pg_query::{NodeEnum, NodeRef, protobuf};

#[test]
fn parses_with_postgres_18_grammar() {
    assert!(typedpg_pg_query::PG_VERSION.starts_with("18."));
    // RETURNING OLD / NEW is PostgreSQL 18 syntax.
    let parsed =
        typedpg_pg_query::parse("UPDATE t SET a = 1 RETURNING WITH (OLD AS o, NEW AS n) o.a, n.a")
            .unwrap();
    assert_eq!(parsed.protobuf.version, typedpg_pg_query::PG_VERSION_NUM);
    assert!(matches!(
        parsed.protobuf.nodes()[0].0,
        NodeRef::UpdateStmt(_)
    ));
}

#[test]
fn parse_errors_carry_the_server_message() {
    let err = typedpg_pg_query::parse("SELECT FROM WHERE").unwrap_err();
    assert_eq!(
        err,
        typedpg_pg_query::Error::Parse("syntax error at or near \"WHERE\"".into())
    );
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
    let nodes = parsed.protobuf.nodes();
    // The statement itself comes first, at depth 0.
    assert!(matches!(nodes[0], (NodeRef::SelectStmt(_), 0)));
    let mut columns: Vec<String> = nodes
        .iter()
        .filter_map(|(n, _)| match n {
            NodeRef::ColumnRef(cr) => cr.fields.last().and_then(|f| match &f.node {
                Some(NodeEnum::String(s)) => Some(s.sval.clone()),
                _ => None,
            }),
            _ => None,
        })
        .collect();
    columns.sort();
    assert_eq!(columns, ["a", "b", "c", "d", "e", "f"]);
    // A subtree walk starts at its own root.
    let stmt = parsed.protobuf.stmts[0]
        .stmt
        .as_ref()
        .unwrap()
        .node
        .as_ref()
        .unwrap();
    assert_eq!(stmt.nodes().len(), nodes.len());
}

#[test]
fn nodes_mut_edits_the_tree_in_place() {
    let mut parsed = typedpg_pg_query::parse("SELECT a FROM t").unwrap();
    for (n, _) in unsafe { parsed.protobuf.nodes_mut() } {
        if let typedpg_pg_query::NodeMut::RangeVar(rv) = n {
            unsafe { (*rv).relname = "renamed".into() };
        }
    }
    assert_eq!(parsed.deparse().unwrap(), "SELECT a FROM renamed");
}

#[test]
fn deparses_trees_and_single_nodes() {
    let parsed = typedpg_pg_query::parse("select a from t where b = 1").unwrap();
    assert_eq!(
        typedpg_pg_query::deparse(&parsed.protobuf).unwrap(),
        "SELECT a FROM t WHERE b = 1"
    );
    let stmt = parsed.protobuf.stmts[0].stmt.as_ref().unwrap();
    assert_eq!(stmt.deparse().unwrap(), "SELECT a FROM t WHERE b = 1");
}

#[test]
fn scans_tokens() {
    let scan = typedpg_pg_query::scan("SELECT $1 FROM t").unwrap();
    let kinds: Vec<protobuf::Token> = scan.tokens.iter().map(|t| t.token()).collect();
    assert_eq!(
        kinds,
        [
            protobuf::Token::Select,
            protobuf::Token::Param,
            protobuf::Token::From,
            protobuf::Token::Ident
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
            Err(typedpg_pg_query::Error::Parse(msg)) => assert_eq!(msg, message, "{sql}"),
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
        let NodeEnum::SelectStmt(sel) = parsed.protobuf.stmts[0]
            .stmt
            .as_ref()
            .unwrap()
            .node
            .clone()
            .unwrap()
        else {
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
