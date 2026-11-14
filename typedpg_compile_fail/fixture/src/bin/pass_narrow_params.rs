//! Compiles cleanly: `bytea`, `jsonb` and `date` parameters (and arrays of
//! them) take their values in every common shape.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    let digest = [7_u8; 32];
    let owned: Vec<u8> = digest.to_vec();
    let slice: &[u8] = &owned;
    let boxed: Box<[u8]> = owned.clone().into_boxed_slice();
    let text = String::from("raw");
    let doc = serde_json::json!({ "a": 1 });
    let day = chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
    typedpg::sql!(
        db,
        "INSERT INTO digests (hash, hashes, doc, docs, day, days)
         VALUES ($hash, $hashes, $doc, $docs, $day, $days)",
        hash = digest,
        hashes = [digest],
        doc = &doc,
        docs = [doc.clone()],
        day = day,
        days = [day],
    )
    .execute()
    .await?;
    typedpg::sql!(
        db,
        "INSERT INTO digests (hash, hashes, doc, docs, day, days)
         VALUES ($hash, $hashes, $doc, $docs, $day, $days)",
        hash = &digest,
        hashes = vec![slice],
        doc = 1,
        docs = vec![true, false],
        day = day,
        days = Vec::<chrono::NaiveDate>::new(),
    )
    .execute()
    .await?;
    typedpg::sql!(
        db,
        "INSERT INTO digests (hash, hashes, doc, docs, day, days)
         VALUES ($hash, $hashes, $doc, $docs, $day, $days)",
        hash = slice,
        hashes = [&owned],
        doc = vec!["a", "b"],
        docs = [serde_json::Value::Null],
        day = day,
        days = vec![day],
    )
    .execute()
    .await?;
    typedpg::sql!(
        db,
        "INSERT INTO digests (hash, hashes, doc, docs, day, days)
         VALUES ($hash, $hashes, $doc, $docs, $day, $days)",
        hash = boxed,
        hashes = vec![owned.clone()],
        doc = serde_json::Value::String(text.clone()),
        docs = [serde_json::Map::new()],
        day = day,
        days = [day],
    )
    .execute()
    .await?;
    typedpg::sql!(
        db,
        "INSERT INTO digests (hash, hashes, doc, docs, day, days)
         VALUES ($hash, $hashes, $doc, $docs, $day, $days)",
        hash = owned,
        hashes = [text.as_bytes()],
        doc = serde_json::Value::Null,
        docs = [1.5],
        day = day,
        days = [day],
    )
    .execute()
    .await?;
    typedpg::sql!(
        db,
        "INSERT INTO digests (hash, hashes, doc, docs, day, days)
         VALUES ($hash, $hashes, $doc, $docs, $day, $days)",
        hash = text.into_bytes(),
        hashes = [b"x"],
        doc = [1, 2],
        docs = Vec::<serde_json::Value>::new(),
        day = day,
        days = [day],
    )
    .execute()
    .await?;
    typedpg::sql!(
        db,
        "SELECT 1 AS one FROM digests WHERE hash = $hash",
        hash = std::collections::VecDeque::from(vec![1_u8]),
    )
    .fetch_all()
    .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
