//! Anonymous records inside arrays, scalar subqueries and CASE get a
//! nested struct, as a top-level `ROW(…)` does — with fields of user
//! types (an enum mapped to a Rust type, a composite).

use typedpg_compile_fail_fixture::Mood;

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    let users = typedpg::sql!(
        db,
        "SELECT array_agg(ROW(name, mood, addr) ORDER BY id) AS users FROM users"
    )
    .fetch_value()
    .await?;
    if let Some(users) = users {
        let _: (&String, &Option<Mood>, Option<&Option<String>>) = (
            &users[0].f1,
            &users[0].f2,
            users[0].f3.as_ref().map(|a| &a.city),
        );
    }

    let pairs = typedpg::sql!(db, "SELECT ARRAY(SELECT ROW(id, age) FROM users) AS pairs")
        .fetch_value()
        .await?;
    let _: Vec<(i64, Option<i32>)> = pairs.into_iter().map(|p| (p.f1, p.f2)).collect();

    let first = typedpg::sql!(
        db,
        "SELECT (SELECT ROW(id, name) FROM users ORDER BY id LIMIT 1) AS first"
    )
    .fetch_value()
    .await?;
    let _: Option<(i64, String)> = first.map(|r| (r.f1, r.f2));

    let picked = typedpg::sql!(
        db,
        "SELECT CASE WHEN age > 18 THEN ROW(id, name) ELSE ROW(0::int8, 'minor'::text) END \
         AS picked FROM users"
    )
    .fetch_all()
    .await?;
    let _: Vec<(i64, String)> = picked
        .into_iter()
        .map(|r| (r.picked.f1, r.picked.f2))
        .collect();
    Ok(())
}

fn main() {
    let _ = case;
}
