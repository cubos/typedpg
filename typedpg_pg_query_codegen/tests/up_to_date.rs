//! The generated sources committed in `typedpg_pg_query` must match what the
//! codegen produces from its libpg_query submodule.

#[test]
fn generated_sources_are_up_to_date() {
    let crate_dir = typedpg_pg_query_codegen::crate_dir();
    for (path, expected) in typedpg_pg_query_codegen::generate(&crate_dir) {
        let actual = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            actual == expected,
            "{} is stale; run `cargo run -p typedpg_pg_query_codegen`",
            path.display()
        );
    }
}
