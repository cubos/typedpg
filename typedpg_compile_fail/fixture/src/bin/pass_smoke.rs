//! Compiles cleanly: proves the fixture's config, migrations and type
//! mappings are sound, so every other case fails for its own reason only.

use typedpg_compile_fail_fixture::{Mood, Prefs};

struct NewUser {
    name: String,
    email: String,
}

#[derive(typedpg::FromRow)]
struct Named {
    name: String,
}

async fn case(db: &tokio_postgres::Client) -> Result<(), typedpg::Error> {
    let id = 1_i64;
    let user = typedpg::sql!(
        db,
        "SELECT name, age, mood, prefs FROM users WHERE id = $id"
    )
    .fetch_one()
    .await?;
    let _: (String, Option<i32>, Option<Mood>, Option<Prefs>) =
        (user.name, user.age, user.mood, user.prefs);

    let users = vec![NewUser {
        name: "a".into(),
        email: "a@example.com".into(),
    }];
    typedpg::sql!(
        db,
        "INSERT INTO users (name, email) VALUES $..users { name, email }"
    )
    .execute()
    .await?;
    typedpg::copy_in!(db, "users (name, email)", users { name, email }).await?;

    let _: Vec<Named> = typedpg::sql!(db, "SELECT name FROM users")
        .fetch_all_as()
        .await?;

    let _ = typedpg::embed_migrations!("./migrations");
    Ok(())
}

fn main() {
    let _ = case;
}
