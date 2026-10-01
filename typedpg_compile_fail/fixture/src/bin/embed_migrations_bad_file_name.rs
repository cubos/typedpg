//! `embed_migrations!` over a directory holding a file that does not follow
//! `NNNN_description.sql`.

fn main() {
    let _ = typedpg::embed_migrations!("./bad_migrations");
}
