//! Two output columns of the same name: the generated struct has one field
//! per name, so the query needs distinct names.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db, "SELECT u.id, p.id FROM users u JOIN users p ON p.id = u.id")
        .fetch_all()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
