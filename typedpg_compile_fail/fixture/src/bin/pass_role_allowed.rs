//! Compiles cleanly: the configured role may run the first statement, and
//! the call-site role of the second may run what the configured role could
//! not (a DELETE that `fixture_app` has no privilege for).

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db = roles, db, "SELECT id, name FROM machines")
        .fetch_all()
        .await?;

    let id = 1_i32;
    typedpg::sql!(
        db = roles,
        role = "fixture_admin",
        db,
        "DELETE FROM machines WHERE id = $id",
        id = id
    )
    .execute()
    .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
