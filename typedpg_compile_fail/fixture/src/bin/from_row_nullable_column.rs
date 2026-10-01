//! A `FromRow` field that can't hold a NULL, for a nullable column.

#[derive(typedpg::FromRow)]
#[allow(dead_code)]
struct Age {
    age: i32,
}

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    let _: Vec<Age> = typedpg::sql!(db, "SELECT age FROM users")
        .fetch_all_as()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
