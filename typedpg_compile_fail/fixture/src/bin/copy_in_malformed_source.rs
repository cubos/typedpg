//! `copy_in!` whose last argument is not `source { field, ... }`: no
//! braces at all, parentheses instead of braces, and braces with no rows
//! before them.

struct NewUser {
    name: String,
}

async fn no_braces(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    let users = vec![NewUser { name: "a".into() }];
    typedpg::copy_in!(db, "users (name)", users).await?;
    Ok(())
}

async fn parentheses(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    let users = vec![NewUser { name: "a".into() }];
    typedpg::copy_in!(db, "users (name)", users(name)).await?;
    Ok(())
}

async fn no_rows(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::copy_in!(db, "users (name)", { name }).await?;
    Ok(())
}

fn main() {
    let _ = (no_braces, parentheses, no_rows);
}
