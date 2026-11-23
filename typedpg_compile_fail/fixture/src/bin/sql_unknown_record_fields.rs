//! A record whose fields nobody knows: a `RETURNS record` function's
//! result (in an array, or as a field of a row), and records of different
//! shapes mixed in one CASE.

async fn in_array(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db, "SELECT ARRAY[untyped_pair()] AS pairs")
        .fetch_all()
        .await?;
    Ok(())
}

async fn as_field(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db, "SELECT ROW(1, untyped_pair()) AS r")
        .fetch_all()
        .await?;
    Ok(())
}

async fn mixed_shapes(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(
        db,
        "SELECT CASE WHEN age > 18 THEN ROW(id, name) ELSE ROW(name) END AS r FROM users"
    )
    .fetch_all()
    .await?;
    Ok(())
}

fn main() {
    let _ = (in_array, as_field, mixed_shapes);
}
