//! A Rust value of the wrong type for a nullable `integer` parameter.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(
        db,
        "UPDATE users SET age = $age WHERE id = 1",
        age = Some("forty")
    )
    .execute()
    .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
