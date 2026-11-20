//! Compiles cleanly: the utility statements a query may be return no rows
//! and run with `.execute()`.

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    typedpg::sql!(db, "SET LOCAL statement_timeout = '5s'")
        .execute()
        .await?;
    typedpg::sql!(db, "LOCK users IN ACCESS SHARE MODE")
        .execute()
        .await?;
    typedpg::sql!(db, "ANALYZE users (name)").execute().await?;
    typedpg::sql!(db, "TRUNCATE users").execute().await?;
    Ok(())
}

fn main() {
    let _ = case;
}
