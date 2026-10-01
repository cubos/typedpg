//! `copy_in!` with a field whose Rust type does not fit its column.

struct NewUser {
    name: String,
    age: String,
}

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    let users = vec![NewUser {
        name: "a".into(),
        age: "old".into(),
    }];
    typedpg::copy_in!(db, "users (name, age)", users { name, age }).await?;
    Ok(())
}

fn main() {
    let _ = case;
}
