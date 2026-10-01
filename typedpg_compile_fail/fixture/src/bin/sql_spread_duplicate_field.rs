//! The same item field listed twice in a `$..spread`.

struct NewUser {
    name: String,
}

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    let users = vec![NewUser { name: "a".into() }];
    typedpg::sql!(
        db,
        "INSERT INTO users (name, email) VALUES $..users { name, name }"
    )
    .execute()
    .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
