//! The configured role may not empty a table it has no TRUNCATE on.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db = roles, db, "TRUNCATE audits")
        .execute()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
