//! The configured role has a column grant, not one for the column read.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db = roles, db, "SELECT id FROM parts")
        .fetch_all()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
