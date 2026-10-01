//! An analyzer error, reported with PG's wording and a snippet of the SQL.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db, "SELECT id, nmae FROM users WHERE age > 3")
        .fetch_all()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
