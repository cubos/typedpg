mod common;

use typedpg::sql;
use typedpg::stream::TryStreamExt;

#[tokio::test]
async fn fetch_stream_yields_the_rows_fetch_all_returns() {
    let pool = common::setup().await;

    let all = sql!(
        &pool,
        "SELECT x, x * 2 AS doubled FROM generate_series(1, 500) AS x ORDER BY x"
    )
    .fetch_all()
    .await
    .expect("fetch_all");
    let streamed: Vec<_> = sql!(
        &pool,
        "SELECT x, x * 2 AS doubled FROM generate_series(1, 500) AS x ORDER BY x"
    )
    .fetch_stream()
    .await
    .expect("fetch_stream")
    .try_collect()
    .await
    .expect("stream rows");

    assert_eq!(streamed.len(), 500);
    for (a, b) in all.iter().zip(&streamed) {
        assert_eq!((a.x, a.doubled), (b.x, b.doubled));
    }
}

#[tokio::test]
async fn a_pooled_stream_keeps_its_connection_until_dropped() {
    let pool = common::setup().await;
    sql!(&pool, "SELECT 1 AS one")
        .fetch_one()
        .await
        .expect("warm up");
    let idle = pool.status().available;

    let mut rows = sql!(&pool, "SELECT x FROM generate_series(1, 3) AS x")
        .fetch_stream()
        .await
        .expect("fetch_stream");
    assert_eq!(
        pool.status().available,
        idle - 1,
        "the stream holds a connection"
    );
    assert_eq!(rows.try_next().await.expect("row").map(|r| r.x), Some(1));

    drop(rows);
    assert_eq!(
        pool.status().available,
        idle,
        "dropping the stream returns it"
    );
}

#[tokio::test]
async fn an_error_while_reading_comes_through_the_stream() {
    // No ORDER BY: a sort would compute every row (and fail) before sending
    // the first one; generate_series emits them as they are computed.
    let pool = common::setup().await;
    let mut rows = sql!(
        &pool,
        "SELECT 10 / (3 - x) AS q FROM generate_series(1, 5) AS x"
    )
    .fetch_stream()
    .await
    .expect("the query starts");

    assert_eq!(rows.try_next().await.expect("row 1").map(|r| r.q), Some(5));
    assert_eq!(rows.try_next().await.expect("row 2").map(|r| r.q), Some(10));
    let err = rows.try_next().await.expect_err("row 3 divides by zero");
    let typedpg::Error::Database(db) = &err else {
        panic!("expected a database error, got {err:?}");
    };
    assert_eq!(
        db.code(),
        Some(&tokio_postgres::error::SqlState::DIVISION_BY_ZERO),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "database error: division by zero (SQLSTATE 22012)"
    );
}

#[tokio::test]
async fn fetch_stream_runs_inside_a_transaction() {
    let pool = common::setup().await;
    let mut client = pool.get().await.expect("client");
    let tx = client.transaction().await.expect("begin");

    let email = "stream-tx@example.com";
    sql!(&tx, "INSERT INTO users (name, email) VALUES ('Tx', $email)")
        .execute()
        .await
        .expect("insert");
    let names: Vec<String> = sql!(&tx, "SELECT name FROM users WHERE email = $email")
        .fetch_stream()
        .await
        .expect("fetch_stream")
        .map_ok(|row| row.name)
        .try_collect()
        .await
        .expect("rows");
    assert_eq!(names, ["Tx"]);
    tx.rollback().await.expect("rollback");
}

struct NewUser {
    name: String,
    email: String,
}

#[tokio::test]
async fn spread_queries_stream_their_returning_rows() {
    let pool = common::setup().await;

    let none: Vec<NewUser> = Vec::new();
    let empty: Vec<_> = sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES $..none { name, email } RETURNING id"
    )
    .fetch_stream()
    .await
    .expect("empty spread")
    .try_collect()
    .await
    .expect("rows");
    assert!(empty.is_empty());

    let users = [
        NewUser {
            name: "S1".into(),
            email: "stream-spread-1@example.com".into(),
        },
        NewUser {
            name: "S2".into(),
            email: "stream-spread-2@example.com".into(),
        },
    ];
    let names: Vec<String> = sql!(
        &pool,
        "INSERT INTO users (name, email) VALUES $..users { name, email } RETURNING name"
    )
    .fetch_stream()
    .await
    .expect("spread")
    .map_ok(|row| row.name)
    .try_collect()
    .await
    .expect("rows");
    assert_eq!(names, ["S1", "S2"]);
}

#[derive(typedpg::FromRow)]
struct Pair {
    x: i32,
    label: String,
}

#[tokio::test]
async fn fetch_stream_as_maps_rows_to_a_from_row_type() {
    let pool = common::setup().await;
    let pairs: Vec<Pair> = sql!(
        &pool,
        "SELECT x, 'n' || x AS label FROM generate_series(1, 3) AS x ORDER BY x"
    )
    .fetch_stream_as::<Pair>()
    .await
    .expect("fetch_stream_as")
    .try_collect()
    .await
    .expect("rows");
    let got: Vec<_> = pairs.iter().map(|p| (p.x, p.label.as_str())).collect();
    assert_eq!(got, [(1, "n1"), (2, "n2"), (3, "n3")]);
}
