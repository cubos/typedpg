//! Two arguments binding the same `$param`: the second would be ignored.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(
        db,
        "SELECT name FROM users WHERE id = $id",
        id = 1i64,
        id = 2i64
    )
    .fetch_all()
    .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
