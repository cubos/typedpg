//! A `$..spread` without the `{ field, ... }` list saying which item
//! fields fill which columns.

struct NewUser {
    name: String,
    email: String,
}

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    let users = vec![NewUser {
        name: "a".into(),
        email: "a@example.com".into(),
    }];
    typedpg::sql!(db, "INSERT INTO users (name, email) VALUES $..users")
        .execute()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
