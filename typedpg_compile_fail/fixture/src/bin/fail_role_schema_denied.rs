//! The configured role has no USAGE on a schema the statement names.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db = roles, db, "SELECT id FROM restricted.secrets")
        .fetch_all()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
