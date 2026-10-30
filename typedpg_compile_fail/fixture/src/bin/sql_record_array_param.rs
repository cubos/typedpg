//! A `record[]` parameter (`ROW(…) = ANY($1)`): PG has no input for an
//! anonymous record, nor for an array of them.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    let rows = ();
    typedpg::sql!(db, "SELECT (id, name) = ANY($rows) AS hit FROM users")
        .fetch_all()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
