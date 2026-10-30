//! A column named `self` is a field named `self_`.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    let rows = typedpg::sql!(db, "SELECT name AS self, id AS crate FROM users")
        .fetch_all()
        .await?;
    for row in rows {
        let _: (&str, i64) = (&row.self_, row.crate_);
    }
    Ok(())
}

fn main() {
    let _ = case;
}
