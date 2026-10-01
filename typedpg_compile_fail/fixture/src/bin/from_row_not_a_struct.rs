//! `#[derive(FromRow)]` on an enum and on a tuple struct.

#[derive(typedpg::FromRow)]
enum Kind {
    A,
}

#[derive(typedpg::FromRow)]
struct Pair(i64, String);

fn main() {}
