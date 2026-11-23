//! A line continuation (`\` at the end of a line) drops the line break and
//! the next line's indentation, as Rust defines it: here it joins `name` and
//! `FROM` into one identifier. The statement checked is the one sent.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(
        db,
        "SELECT name\
           FROM users"
    )
    .fetch_all()
    .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
