//! A parameter whose name is not a Rust identifier: there is no variable
//! to bind it to.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db, "SELECT id FROM users WHERE name = $€")
        .fetch_all()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
