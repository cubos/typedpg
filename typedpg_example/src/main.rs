//! Example demonstrating typedpg with static analysis.
//!
//! The `sql!` macro analyzes queries at compile time using the embedded
//! PostgreSQL seed and the migration files — no Docker daemon needed.
//!
//! To build: `cargo build -p typedpg_example`

use typedpg::sql;

/// Example: compile-time verified queries against a blog schema.
///
/// These functions demonstrate that the `sql!` macro correctly infers types
/// from the migrations without needing a live PostgreSQL instance.
#[allow(dead_code)]
async fn example_queries(pool: &deadpool_postgres::Pool) -> Result<(), typedpg::Error> {
    // SELECT with typed output columns.
    let users = sql!(pool, "SELECT id, name, email, age FROM users")
        .fetch_all()
        .await?;
    for user in &users {
        println!(
            "User #{}: {} ({}) age={:?}",
            user.id, user.name, user.email, user.age
        );
    }

    // SELECT with WHERE parameter.
    let user_id: i64 = 1;
    let user = sql!(
        pool,
        "SELECT id, name, email FROM users WHERE id = $user_id"
    )
    .fetch_optional()
    .await?;
    if let Some(u) = user {
        println!("Found: {} <{}>", u.name, u.email);
    }

    // INSERT with RETURNING.
    let name = "Alice";
    let email = "alice@example.com";
    let inserted = sql!(
        pool,
        "INSERT INTO users (name, email) VALUES ($name, $email) RETURNING id, created_at"
    )
    .fetch_one()
    .await?;
    println!("Inserted user #{} at {}", inserted.id, inserted.created_at);

    // JOIN query.
    let posts = sql!(
        pool,
        "SELECT p.id, p.title, u.name as author_name
         FROM posts p
         INNER JOIN users u ON u.id = p.user_id
         WHERE p.user_id = $user_id"
    )
    .fetch_all()
    .await?;
    for post in &posts {
        println!(
            "Post #{}: '{}' by {}",
            post.id, post.title, post.author_name
        );
    }

    // UPDATE with execute().
    let title = "Updated Title";
    let post_id: i64 = 1;
    let affected = sql!(pool, "UPDATE posts SET title = $title WHERE id = $post_id")
        .execute()
        .await?;
    println!("Updated {affected} post(s)");

    // DELETE with execute().
    let deleted = sql!(pool, "DELETE FROM comments WHERE post_id = $post_id")
        .execute()
        .await?;
    println!("Deleted {deleted} comment(s)");

    // Aggregate with LEFT JOIN — demonstrates nullability tracking.
    let stats = sql!(
        pool,
        "SELECT u.id, u.name, COUNT(p.id) as post_count
         FROM users u
         LEFT JOIN posts p ON p.user_id = u.id
         GROUP BY u.id"
    )
    .fetch_all()
    .await?;
    for s in &stats {
        println!("User {}: {} posts", s.name, s.post_count);
    }

    Ok(())
}

fn main() {
    println!("typedpg_example compiled successfully!");
    println!("All sql!() macros were verified at compile time using static analysis.");
    println!("No Docker daemon was required.");
}
