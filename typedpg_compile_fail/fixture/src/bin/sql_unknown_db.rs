//! `db = name` naming no `[package.metadata.typedpg.databases.<name>]`.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db = nosuch, db, "SELECT 1 AS one")
        .fetch_all()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
