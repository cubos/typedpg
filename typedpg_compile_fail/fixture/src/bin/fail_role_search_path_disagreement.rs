//! The two configured roles resolve `items` to tables whose `x` columns
//! have different types, so the code one generated analysis would describe
//! cannot be right for both.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db = searchpath, db, "SELECT x FROM items")
        .fetch_all()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
