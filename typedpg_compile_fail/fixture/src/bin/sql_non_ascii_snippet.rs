//! The snippet's marker sits under the token on screen when characters of
//! more than one byte precede it on its line.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db, "SELECT 'ação' AS rótulo, nme FROM users")
        .fetch_all()
        .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
