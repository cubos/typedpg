//! A PG type with neither a built-in nor a configured Rust mapping.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db, "SELECT to_tsvector('simple', name) AS v FROM users")
        .fetch_all()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
