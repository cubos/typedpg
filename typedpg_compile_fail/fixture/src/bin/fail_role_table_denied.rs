//! The configured role may not read a table it has no privilege on.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db = roles, db, "DELETE FROM audits")
        .fetch_all()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
