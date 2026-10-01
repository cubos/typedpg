//! A `FromRow` field the query has no column for.

#[derive(typedpg::FromRow)]
#[allow(dead_code)]
struct Named {
    name: String,
    nickname: String,
}

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    let _: Named = typedpg::sql!(db, "SELECT name FROM users LIMIT 1")
        .fetch_one_as()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
