//! `role =` takes a string literal: the role's name.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db = roles, role = 42, db, "SELECT id FROM machines")
        .fetch_all()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
