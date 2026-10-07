//! An element of a `x IN $..list` spread whose type isn't the one the
//! IN's left side requires.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    let ids = vec!["1".to_string(), "2".to_string()];
    typedpg::sql!(db, "SELECT name FROM users WHERE id IN $..ids")
        .fetch_all()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
