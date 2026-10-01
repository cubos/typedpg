//! An assignment naming no `$param` of the SQL.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db, "SELECT name FROM users WHERE id = $id", id = 1, idd = 2)
        .fetch_all()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
