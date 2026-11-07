//! The crate `cargo typedpg lint`'s integration test scans: nothing here is
//! built, only read.

use typedpg::{copy_in, sql};

pub async fn run(pool: &typedpg::Pool) -> Result<(), Box<dyn std::error::Error>> {
    // The escape comes before the finding: its span maps through it.
    sql!(pool, "SELECT id\nFROM users WHERE email = NULL").fetch_all().await?;
    // A raw string before the finding: no escapes to decode.
    sql!(pool, r#"SELECT id FROM users WHERE email = NULL"#).fetch_all().await?;
    // The subquery's title may be NULL.
    sql!(pool, "SELECT id FROM users WHERE email NOT IN (SELECT title FROM posts)")
        .fetch_all()
        .await?;
    // Every row of users.
    sql!(pool, "UPDATE users SET name = 'x'").execute().await?;
    // The second database, with its own migrations.
    sql!(db = other, pool, "SELECT id FROM things WHERE name = NULL").fetch_all().await?;
    // The clean spelling.
    sql!(pool, "SELECT id FROM users WHERE email IS NULL").fetch_all().await?;
    // A binding the parameter captures without it.
    sql!(pool, "SELECT id FROM users WHERE id = $id", id = id)
        .fetch_all()
        .await?;
    // An analysis error: there is no such column.
    sql!(pool, "SELECT nope FROM users").fetch_all().await?;
    // A COPY target that does not analyze: there is no such table.
    copy_in!(pool, "missing (id)", rows { id }).execute().await?;
    // The macro's own error: the SQL is not a string literal.
    let query = "SELECT id FROM users";
    sql!(pool, query).fetch_all().await?;
    Ok(())
}
