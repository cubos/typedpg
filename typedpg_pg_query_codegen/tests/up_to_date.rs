//! The generated sources committed in `typedpg_pg_query` must match what the
//! codegen produces from the PostgreSQL headers it imports.

#[test]
fn generated_sources_are_up_to_date() {
    let crate_dir = typedpg_pg_query_codegen::crate_dir();
    let generated = typedpg_pg_query_codegen::generate_from_headers(
        &crate_dir.join("postgres/include"),
        &crate_dir.join("postgres/src/backend/parser/gram.h"),
    );
    for (path, expected) in generated {
        let path = crate_dir.join(path);
        let actual = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            actual == expected,
            "{} is stale; run `cargo run -p typedpg_pg_query_codegen`",
            path.display()
        );
    }
}
