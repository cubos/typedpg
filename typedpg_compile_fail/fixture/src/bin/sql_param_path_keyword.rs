//! A parameter named `self`: `self` has no raw form, so no variable can be
//! named so to hold the parameter's value.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db, "SELECT id FROM users WHERE name = $self")
        .fetch_all()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
