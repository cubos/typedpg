//! `cargo run -p typedpg_pg_query_codegen` rewrites the generated sources of
//! `typedpg_pg_query` from the PostgreSQL headers it imports
//! (`postgres/include` and `postgres/src/backend/parser/gram.h`).

fn main() {
    let crate_dir = typedpg_pg_query_codegen::crate_dir();
    let generated = typedpg_pg_query_codegen::generate_from_headers(
        &crate_dir.join("postgres/include"),
        &crate_dir.join("postgres/src/backend/parser/gram.h"),
    );
    for (path, content) in generated {
        let path = crate_dir.join(path);
        std::fs::write(&path, content)
            .unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
        println!("wrote {}", path.display());
    }
}
