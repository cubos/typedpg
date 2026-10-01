//! A Rust value of the wrong type for a `bigint` parameter.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db, "SELECT name FROM users WHERE id = $id", id = "one")
        .fetch_all()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
