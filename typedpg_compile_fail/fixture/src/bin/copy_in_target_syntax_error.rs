//! A `copy_in!` target the grammar rejects: the error is about the target
//! as written, not the `COPY ... FROM STDIN` statement built around it.

struct NewUser {
    name: String,
}

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    let users = vec![NewUser { name: "a".into() }];
    typedpg::copy_in!(db, "users (name email)", users { name }).await?;
    Ok(())
}

fn main() {
    let _ = case;
}
