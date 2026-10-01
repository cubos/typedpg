//! Misspelled function names get the intended one suggested (`lenght`
//! is one transposition from `length`); a known name called with the
//! wrong arguments lists the overloads instead.

async fn typo(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db, "SELECT lenght(name) AS n FROM users")
        .fetch_all()
        .await?;
    Ok(())
}

async fn no_overload(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db, "SELECT length(name, 1, 2) AS n FROM users")
        .fetch_all()
        .await?;
    Ok(())
}

fn main() {
    let _ = (typo, no_overload);
}
