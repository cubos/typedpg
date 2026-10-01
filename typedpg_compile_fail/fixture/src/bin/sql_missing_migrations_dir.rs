//! A configured migrations directory that does not exist counts as no
//! migrations; the analysis error says so, or a typo in the path would
//! only show up as every table missing.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db = missing, db, "SELECT id FROM users")
        .fetch_all()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
