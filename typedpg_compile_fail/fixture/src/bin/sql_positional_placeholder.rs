//! A native `$1` placeholder has no name an argument could bind to.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db, "SELECT name FROM users WHERE id = $1")
        .fetch_all()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
