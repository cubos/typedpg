//! Text or an `Option` passed where `jsonb` takes a JSON value:
//! `serde_json::Value` converts from `String` / `&str` (a JSON string, so
//! JSON text would be stored quoted) and from `Option` (`None` as JSON
//! `null`, past the NOT NULL).

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    let text = String::from(r#"{"a": 1}"#);
    typedpg::sql!(
        db,
        "SELECT 1 AS one FROM digests WHERE doc = $doc",
        doc = text
    )
    .fetch_all()
    .await?;
    typedpg::sql!(
        db,
        "SELECT 1 AS one FROM digests WHERE doc = $doc",
        doc = "{}"
    )
    .fetch_all()
    .await?;
    let missing: Option<serde_json::Value> = None;
    typedpg::sql!(
        db,
        "SELECT 1 AS one FROM digests WHERE doc = $doc",
        doc = missing
    )
    .fetch_all()
    .await?;
    typedpg::sql!(
        db,
        "SELECT 1 AS one FROM digests WHERE docs = $docs",
        docs = ["{}"],
    )
    .fetch_all()
    .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
