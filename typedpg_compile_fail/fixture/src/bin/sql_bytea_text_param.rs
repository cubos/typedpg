//! Text passed where `bytea` takes bytes: `Vec<u8>` converts from `String`
//! and `&str`, which would bind a hex digest silently as its UTF-8 bytes.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    let hex = String::from("00ff");
    typedpg::sql!(
        db,
        "SELECT 1 AS one FROM digests WHERE hash = $hash",
        hash = hex
    )
    .fetch_all()
    .await?;
    typedpg::sql!(
        db,
        "SELECT 1 AS one FROM digests WHERE hash = $hash",
        hash = "00ff"
    )
    .fetch_all()
    .await?;
    typedpg::sql!(
        db,
        "SELECT 1 AS one FROM digests WHERE hashes = $hashes",
        hashes = ["00ff", "ff00"],
    )
    .fetch_all()
    .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
