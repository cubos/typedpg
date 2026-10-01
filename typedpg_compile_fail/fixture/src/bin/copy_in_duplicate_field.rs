//! `copy_in!` listing the same field twice.

struct NewUser {
    name: String,
}

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    let users = vec![NewUser { name: "a".into() }];
    typedpg::copy_in!(db, "users (name, email)", users { name, name }).await?;
    Ok(())
}

fn main() {
    let _ = case;
}
