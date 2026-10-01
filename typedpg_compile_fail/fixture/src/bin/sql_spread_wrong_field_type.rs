//! A `$..spread` item field of the wrong type for its column.

struct NewUser {
    name: String,
    email: String,
    age: String,
}

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    let users: Vec<NewUser> = Vec::new();
    typedpg::sql!(
        db,
        "INSERT INTO users (name, email, age) VALUES $..users { name, email, age }"
    )
    .execute()
    .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
