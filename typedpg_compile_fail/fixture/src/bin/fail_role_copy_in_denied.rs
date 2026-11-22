//! `copy_in!` is checked against the configured role, as `sql!` is: it may
//! not insert into a table it only reads.

struct Machine {
    id: i32,
    name: String,
}

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    let rows = vec![Machine {
        id: 1,
        name: "a".into(),
    }];
    typedpg::copy_in!(db = roles, db, "machines (id, name)", rows { id, name }).await?;
    Ok(())
}

fn main() {
    let _ = case;
}
