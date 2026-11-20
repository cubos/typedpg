//! Compiles cleanly: the call-site role's search path makes `x` a `text`
//! column, and the generated row field is a `String` — not the `i32` the
//! public table's `x` is.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    let row = typedpg::sql!(db = searchpath, role = "sp_alt", db, "SELECT x FROM items")
        .fetch_one()
        .await?;
    let x: String = row.x;
    let _ = x;
    Ok(())
}

fn main() {
    let _ = case;
}
