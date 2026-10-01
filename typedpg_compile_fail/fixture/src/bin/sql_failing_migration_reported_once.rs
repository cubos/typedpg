//! A failing migration fails every `sql!` of the crate: the first reports
//! it in full, located in the migration file; the others in one line.

async fn first(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db = broken, db, "SELECT id FROM things")
        .fetch_all()
        .await?;
    Ok(())
}

async fn second(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db = broken, db, "SELECT name FROM things")
        .fetch_all()
        .await?;
    Ok(())
}

fn main() {
    let _ = (first, second);
}
