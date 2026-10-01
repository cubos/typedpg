//! A composite-typed query parameter: the macro has no OID to bind it with.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    let addr = ();
    typedpg::sql!(db, "UPDATE users SET addr = $addr WHERE id = 1")
        .execute()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
