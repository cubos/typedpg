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
