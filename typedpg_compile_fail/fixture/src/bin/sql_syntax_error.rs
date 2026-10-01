//! A grammar error, with a caret at the position PG reports — on the
//! source line it is on in a multi-line literal.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(
        db,
        "SELECT id, name \
           FROM users \
          WHERE id = = $id",
        id = 1i64
    )
    .fetch_all()
    .await?;
    Ok(())
}

fn main() {
    let _ = case;
}
