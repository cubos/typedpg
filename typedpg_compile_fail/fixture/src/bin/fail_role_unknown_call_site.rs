//! A call-site role the migrations do not create: typedpg can only check the
//! roles they do.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db = roles, role = "nobody", db, "SELECT id FROM machines")
        .fetch_all()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
