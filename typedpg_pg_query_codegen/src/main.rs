//! `cargo run -p typedpg_pg_query_codegen` rewrites the generated sources of
//! `typedpg_pg_query` after its libpg_query submodule moves.

fn main() {
    let crate_dir = typedpg_pg_query_codegen::crate_dir();
    for (path, content) in typedpg_pg_query_codegen::generate(&crate_dir) {
        std::fs::write(&path, content)
            .unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
        println!("wrote {}", path.display());
    }
}
