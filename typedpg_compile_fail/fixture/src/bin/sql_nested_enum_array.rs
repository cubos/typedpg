//! An array of a domain over an enum array: arrays of arrays of an enum
//! are not decodable.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db, "SELECT mood_history FROM users")
        .fetch_all()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
