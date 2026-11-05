//! Ignored by `lint.ignore`: its findings are dropped, its failure is not.

use typedpg::sql;

pub async fn ignored(pool: &typedpg::Pool) -> Result<(), Box<dyn std::error::Error>> {
    // Matched by `ignore = ["tests/ignored.rs"]`: never reported.
    sql!(pool, "UPDATE users SET name = 'x'").execute().await?;
    // A failure is never ignored: the query still fails lint.
    sql!(pool, "SELECT nope FROM users").fetch_all().await?;
    Ok(())
}
