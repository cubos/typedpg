//! A `NaiveDateTime` passed where `date` takes a `NaiveDate`: `NaiveDate`
//! converts from it, dropping the time silently.

async fn case(
    db: &tokio_postgres::Client,
    at: chrono::NaiveDateTime,
) -> Result<(), typedpg::Error> {
    typedpg::sql!(
        db,
        "SELECT 1 AS one FROM digests WHERE day = $day",
        day = at
    )
    .fetch_all()
    .await?;
    typedpg::sql!(
        db,
        "SELECT 1 AS one FROM digests WHERE days = $days",
        days = [at]
    )
    .fetch_all()
    .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
