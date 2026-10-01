//! `embed_migrations!` pointed at a directory that does not exist.

fn main() {
    let _ = typedpg::embed_migrations!("./no_such_dir");
}
