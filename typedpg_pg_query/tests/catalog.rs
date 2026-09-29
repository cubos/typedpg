//! PL/pgSQL compiled against a caller-supplied catalog.

use typedpg_pg_query::{Catalog, CatalogAttribute, CatalogType, parse_plpgsql_with_catalog};

const PG_CATALOG: u32 = 11;
const PUBLIC: u32 = 2200;
const APP: u32 = 16_384;

/// A tiny catalog: a few built-in types, a schema `app` with an enum `mood`
/// (and its array), and a table `public.users (id int4, name text)`.
struct Mini;

fn ty(oid: u32, name: &str, namespace: u32, len: i16, typtype: u8, category: u8) -> CatalogType {
    CatalogType {
        oid,
        name: name.into(),
        namespace,
        len,
        by_val: len > 0,
        typtype,
        category,
        preferred: false,
        align: if len == -1 { b'i' } else { b'c' },
        relid: 0,
        subscript: 0,
        elem: 0,
        array: 0,
        base_type: 0,
        typmod: -1,
        not_null: false,
        collation: 0,
    }
}

impl Mini {
    fn types() -> Vec<CatalogType> {
        let array = |oid, name: &str, elem| CatalogType {
            elem,
            subscript: 6179, // array_subscript_handler
            ..ty(oid, name, PG_CATALOG, -1, b'b', b'A')
        };
        vec![
            ty(16, "bool", PG_CATALOG, 1, b'b', b'B'),
            CatalogType {
                array: 1007,
                align: b'i',
                ..ty(23, "int4", PG_CATALOG, 4, b'b', b'N')
            },
            CatalogType {
                collation: 100,
                array: 1009,
                ..ty(25, "text", PG_CATALOG, -1, b'b', b'S')
            },
            ty(2249, "record", PG_CATALOG, -1, b'p', b'P'),
            ty(2278, "void", PG_CATALOG, 4, b'p', b'P'),
            ty(2279, "trigger", PG_CATALOG, 4, b'p', b'P'),
            array(1007, "_int4", 23),
            array(1009, "_text", 25),
            CatalogType {
                array: 16_386,
                align: b'i',
                ..ty(16_385, "mood", APP, 4, b'e', b'E')
            },
            array(16_386, "_mood", 16_385),
            CatalogType {
                relid: 16_388,
                align: b'd',
                ..ty(16_387, "users", PUBLIC, -1, b'c', b'C')
            },
        ]
    }
}

impl Catalog for Mini {
    fn type_by_oid(&self, oid: u32) -> Option<CatalogType> {
        Self::types().into_iter().find(|t| t.oid == oid)
    }
    fn type_by_name(&self, namespace: u32, name: &str) -> Option<u32> {
        Self::types()
            .into_iter()
            .find(|t| t.namespace == namespace && t.name == name)
            .map(|t| t.oid)
    }
    fn namespace_by_name(&self, name: &str) -> Option<u32> {
        match name {
            "pg_catalog" => Some(PG_CATALOG),
            "public" => Some(PUBLIC),
            "app" => Some(APP),
            _ => None,
        }
    }
    fn namespace_name(&self, namespace: u32) -> Option<String> {
        match namespace {
            PG_CATALOG => Some("pg_catalog".into()),
            PUBLIC => Some("public".into()),
            APP => Some("app".into()),
            _ => None,
        }
    }
    fn search_path(&self) -> Vec<u32> {
        vec![PG_CATALOG, PUBLIC]
    }
    fn relation_by_name(&self, namespace: u32, name: &str) -> Option<u32> {
        (namespace == PUBLIC && name == "users").then_some(16_388)
    }
    fn relation_type(&self, relation: u32) -> Option<u32> {
        (relation == 16_388).then_some(16_387)
    }
    fn attribute_by_name(&self, relation: u32, name: &str) -> Option<CatalogAttribute> {
        if relation != 16_388 {
            return None;
        }
        match name {
            "id" => Some(CatalogAttribute {
                number: 1,
                type_oid: 23,
                typmod: -1,
                collation: 0,
            }),
            "name" => Some(CatalogAttribute {
                number: 2,
                type_oid: 25,
                typmod: -1,
                collation: 100,
            }),
            _ => None,
        }
    }
    fn attribute_by_number(&self, relation: u32, number: i16) -> Option<CatalogAttribute> {
        ["id", "name"]
            .get(usize::try_from(number - 1).ok()?)
            .and_then(|n| self.attribute_by_name(relation, n))
    }
}

fn datums(json: &serde_json::Value) -> Vec<serde_json::Value> {
    json[0]["PLpgSQL_function"]["datums"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn compile(body: &str) -> Result<serde_json::Value, String> {
    parse_plpgsql_with_catalog(
        &format!("CREATE FUNCTION f() RETURNS int4 LANGUAGE plpgsql AS $${body}$$"),
        &Mini,
    )
    .map_err(|e| e.to_string())
}

#[test]
fn user_types_resolve_with_their_real_kind() {
    // An enum in another schema is a scalar; its array is a true array.
    let json = compile("DECLARE m app.mood; ms app.mood[]; BEGIN RETURN 1; END").unwrap();
    let d = datums(&json);
    assert!(d[1]["PLpgSQL_var"]["refname"] == "m", "{json}");
    assert_eq!(
        d[1]["PLpgSQL_var"]["datatype"]["PLpgSQL_type"]["typname"],
        "mood"
    );
    assert_eq!(
        d[2]["PLpgSQL_var"]["datatype"]["PLpgSQL_type"]["typname"],
        "_mood"
    );
}

#[test]
fn rowtype_and_column_type_references_resolve() {
    let json = compile("DECLARE r users%ROWTYPE; n users.name%TYPE; BEGIN RETURN 1; END").unwrap();
    let d = datums(&json);
    assert!(d[1].get("PLpgSQL_rec").is_some(), "{json}");
    assert_eq!(
        d[2]["PLpgSQL_var"]["datatype"]["PLpgSQL_type"]["typname"],
        "text"
    );
}

#[test]
fn missing_objects_get_postgres_errors() {
    for (body, message) in [
        (
            "DECLARE x nosuch; BEGIN RETURN 1; END",
            "type \"nosuch\" does not exist",
        ),
        (
            "DECLARE x nope.mood; BEGIN RETURN 1; END",
            "schema \"nope\" does not exist",
        ),
        (
            "DECLARE r nosuch%ROWTYPE; BEGIN RETURN 1; END",
            "relation \"nosuch\" does not exist",
        ),
        (
            "DECLARE n users.nope%TYPE; BEGIN RETURN 1; END",
            "column \"nope\" of relation \"users\" does not exist",
        ),
        (
            "DECLARE n y%TYPE; BEGIN RETURN 1; END",
            "variable \"y\" does not exist",
        ),
    ] {
        let err = compile(body).unwrap_err();
        assert!(err.contains(message), "{body}: {err}");
    }
}

#[test]
fn signature_types_come_from_the_catalog() {
    let json = parse_plpgsql_with_catalog(
        "CREATE FUNCTION f(m app.mood, VARIADIC xs int4[]) RETURNS app.mood LANGUAGE plpgsql \
         AS $$ BEGIN RETURN m; END $$",
        &Mini,
    )
    .unwrap();
    assert_eq!(
        datums(&json)[0]["PLpgSQL_var"]["datatype"]["PLpgSQL_type"]["typname"],
        "mood"
    );
    let err = parse_plpgsql_with_catalog(
        "CREATE FUNCTION f() RETURNS nosuch LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$",
        &Mini,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("type \"nosuch\" does not exist"),
        "{err}"
    );
}

#[test]
fn a_panicking_catalog_panics_the_caller() {
    struct Panics;
    impl Catalog for Panics {
        fn type_by_oid(&self, _: u32) -> Option<CatalogType> {
            panic!("catalog bug")
        }
        fn type_by_name(&self, _: u32, _: &str) -> Option<u32> {
            panic!("catalog bug")
        }
        fn namespace_by_name(&self, _: &str) -> Option<u32> {
            panic!("catalog bug")
        }
        fn namespace_name(&self, _: u32) -> Option<String> {
            panic!("catalog bug")
        }
        fn search_path(&self) -> Vec<u32> {
            panic!("catalog bug")
        }
        fn relation_by_name(&self, _: u32, _: &str) -> Option<u32> {
            panic!("catalog bug")
        }
        fn relation_type(&self, _: u32) -> Option<u32> {
            panic!("catalog bug")
        }
        fn attribute_by_name(&self, _: u32, _: &str) -> Option<CatalogAttribute> {
            panic!("catalog bug")
        }
        fn attribute_by_number(&self, _: u32, _: i16) -> Option<CatalogAttribute> {
            panic!("catalog bug")
        }
    }
    let caught = std::panic::catch_unwind(|| {
        let _ = parse_plpgsql_with_catalog(
            "CREATE FUNCTION f() RETURNS int4 LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$",
            &Panics,
        );
    });
    assert!(caught.is_err());
    // The thread is usable afterwards, without the catalog.
    assert!(
        typedpg_pg_query::parse_plpgsql(
            "CREATE FUNCTION f() RETURNS int4 LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$"
        )
        .is_ok()
    );
}
