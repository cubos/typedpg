//! A call-site role replaces the configured one, and this one is stricter:
//! the configured role may read `machines`, `fixture_auditor` may not.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(role = "fixture_auditor", db = roles, db, "SELECT id FROM machines")
        .fetch_all()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
