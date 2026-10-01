//! A migration of the selected database that the DDL interpreter rejects.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db = broken, db, "SELECT id FROM things")
        .fetch_all()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
