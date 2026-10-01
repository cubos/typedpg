//! An `Option` bound to a parameter the analyzer infers as NOT NULL.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    let id: Option<i64> = Some(1);
    typedpg::sql!(db, "SELECT name FROM users WHERE id = $id")
        .fetch_all()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
