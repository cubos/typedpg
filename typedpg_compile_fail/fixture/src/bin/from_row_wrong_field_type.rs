//! A `FromRow` field of another type than its column's (`bigint`).

#[derive(typedpg::FromRow)]
#[allow(dead_code)]
struct Id {
    id: i32,
}

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    let _: Option<Id> = typedpg::sql!(db, "SELECT id FROM users LIMIT 1")
        .fetch_optional_as()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
