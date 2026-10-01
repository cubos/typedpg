//! The same queries through every executor: deadpool `Pool` and `Object`,
//! a bb8 pool, a raw `tokio_postgres::Client` and a `Transaction`; the
//! default `Executor::copy_in`; and pool failures surfacing as
//! `Error::Pool`.

mod common;

use std::time::Duration;

use tokio_postgres::NoTls;
use typedpg::stream::TryStreamExt;
use typedpg::{Executor, copy_in, sql};

struct NewUser {
    name: String,
    email: String,
}

/// fetch_all / fetch_one / fetch_optional / fetch_stream / execute and
/// copy_in! through `ex`, on rows keyed by a run-unique tag.
async fn exercise<E: Executor + Sync>(ex: &E, kind: &str) {
    let tag = common::unique(kind);
    let users: Vec<NewUser> = (0..3)
        .map(|i| NewUser {
            name: format!("{kind}-{i}"),
            email: format!("{tag}-{i}@example.com"),
        })
        .collect();
    let copied = copy_in!(ex, "users (name, email)", users { name, email })
        .await
        .expect("copy_in");
    assert_eq!(copied, 3, "{kind}");

    let pattern_owned = format!("{tag}-%");
    let pattern = pattern_owned.as_str();
    let all = sql!(
        ex,
        "SELECT name FROM users WHERE email LIKE $pattern ORDER BY email"
    )
    .fetch_all()
    .await
    .expect("fetch_all");
    let names: Vec<_> = all.iter().map(|r| r.name.as_str()).collect();
    let expected: Vec<_> = (0..3).map(|i| format!("{kind}-{i}")).collect();
    assert_eq!(names, expected, "{kind}");

    let email_owned = format!("{tag}-1@example.com");
    let email = email_owned.as_str();
    let one = sql!(ex, "SELECT name FROM users WHERE email = $email")
        .fetch_one()
        .await
        .expect("fetch_one");
    assert_eq!(one.name, format!("{kind}-1"));

    let missing_owned = format!("{tag}-none@example.com");
    let missing = missing_owned.as_str();
    let none = sql!(ex, "SELECT name FROM users WHERE email = $missing")
        .fetch_optional()
        .await
        .expect("fetch_optional");
    assert!(none.is_none(), "{kind}");

    let streamed: Vec<_> = sql!(
        ex,
        "SELECT name FROM users WHERE email LIKE $pattern ORDER BY email"
    )
    .fetch_stream()
    .await
    .expect("fetch_stream")
    .try_collect()
    .await
    .expect("stream rows");
    assert_eq!(streamed.len(), 3, "{kind}");

    let age = 7;
    let updated = sql!(ex, "UPDATE users SET age = $age WHERE email LIKE $pattern")
        .execute()
        .await
        .expect("execute");
    assert_eq!(updated, 3, "{kind}");
}

#[tokio::test]
async fn deadpool_pool() {
    let pool = common::setup().await;
    exercise(&pool, "deadpool-pool").await;
}

#[tokio::test]
async fn deadpool_object() {
    let pool = common::setup().await;
    let object = pool.get().await.expect("object");
    exercise(&object, "deadpool-object").await;
}

#[tokio::test]
async fn deadpool_transaction() {
    let pool = common::setup().await;
    let mut object = pool.get().await.expect("object");
    let tx = object.transaction().await.expect("begin");
    exercise(&tx, "deadpool-tx").await;
    tx.commit().await.expect("commit");
}

#[tokio::test]
async fn tokio_postgres_client() {
    let config = common::database_config().await;
    let (client, conn) = config.connect(NoTls).await.expect("connect");
    tokio::spawn(conn);
    exercise(&client, "client").await;
}

/// Inside a transaction, and rolled back: nothing it wrote stays.
#[tokio::test]
async fn tokio_postgres_transaction() {
    let config = common::database_config().await;
    let (mut client, conn) = config.connect(NoTls).await.expect("connect");
    tokio::spawn(conn);
    let tx = client.transaction().await.expect("begin");
    exercise(&tx, "transaction").await;
    tx.rollback().await.expect("rollback");

    let pattern = "transaction-%";
    let left = sql!(
        &client,
        "SELECT count(*) AS n FROM users WHERE name LIKE $pattern"
    )
    .fetch_value()
    .await
    .expect("count");
    assert_eq!(left, 0);
}

#[tokio::test]
async fn bb8_pool() {
    let config = common::database_config().await;
    let manager = bb8_postgres::PostgresConnectionManager::new(config, NoTls);
    let pool = bb8::Pool::builder()
        .max_size(2)
        .build(manager)
        .await
        .expect("bb8 pool");
    exercise(&pool, "bb8").await;
}

/// An executor implementing only the required methods: `copy_in!` gets the
/// trait's default, `Error::Unsupported`, while queries still work.
struct QueryOnly(tokio_postgres::Client);

impl Executor for QueryOnly {
    async fn query<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn tokio_postgres::types::ToSql + Sync)],
    ) -> Result<Vec<tokio_postgres::Row>, typedpg::Error> {
        Ok(self.0.query(sql, params).await?)
    }

    async fn execute<'a>(
        &'a self,
        sql: &'a str,
        params: &'a [&'a (dyn tokio_postgres::types::ToSql + Sync)],
    ) -> Result<u64, typedpg::Error> {
        Ok(self.0.execute(sql, params).await?)
    }
}

#[tokio::test]
async fn default_copy_in_is_unsupported() {
    let config = common::database_config().await;
    let (client, conn) = config.connect(NoTls).await.expect("connect");
    tokio::spawn(conn);
    let ex = QueryOnly(client);

    let one = sql!(&ex, "SELECT 1 AS one")
        .fetch_one()
        .await
        .expect("query");
    assert_eq!(one.one, 1);
    // The default query_stream collects the rows first.
    let rows: Vec<_> = sql!(&ex, "SELECT x FROM generate_series(1, 3) AS x")
        .fetch_stream()
        .await
        .expect("fetch_stream")
        .try_collect()
        .await
        .expect("rows");
    assert_eq!(rows.len(), 3);

    let users = [NewUser {
        name: "unsupported".into(),
        email: format!("{}@example.com", common::unique("unsupported")),
    }];
    let err = copy_in!(&ex, "users (name, email)", users { name, email })
        .await
        .expect_err("no COPY");
    assert!(
        matches!(err, typedpg::Error::Unsupported(_)),
        "unexpected error: {err:?}"
    );
    assert_eq!(
        err.to_string(),
        "unsupported: this executor does not implement COPY"
    );
}

/// A deadpool pool whose only connection is checked out times out waiting.
#[tokio::test]
async fn exhausted_deadpool_pool_is_a_pool_error() {
    let server = common::server().await;
    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some(server.host().into());
    cfg.port = Some(server.port());
    cfg.user = Some("postgres".into());
    cfg.password = Some("postgres".into());
    cfg.dbname = Some(common::DATABASE.into());
    let mut pool_cfg = deadpool_postgres::PoolConfig::new(1);
    pool_cfg.timeouts.wait = Some(Duration::from_millis(200));
    cfg.pool = Some(pool_cfg);
    let pool = cfg
        .create_pool(Some(deadpool_postgres::Runtime::Tokio1), NoTls)
        .expect("pool");

    let _held = pool.get().await.expect("the only connection");
    let err = sql!(&pool, "SELECT 1 AS one")
        .fetch_one()
        .await
        .expect_err("exhausted");
    assert!(
        matches!(err, typedpg::Error::Pool(_)),
        "unexpected error: {err:?}"
    );
    assert!(err.to_string().starts_with("pool error: "), "{err}");
}

/// A pool whose server cannot be reached reports `Error::Pool` too.
#[tokio::test]
async fn unreachable_pools_are_pool_errors() {
    // A port nothing listens on: bind one, then free it.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("free port")
        .port();

    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some("127.0.0.1".into());
    cfg.port = Some(port);
    cfg.user = Some("postgres".into());
    cfg.dbname = Some("postgres".into());
    let pool = cfg
        .create_pool(Some(deadpool_postgres::Runtime::Tokio1), NoTls)
        .expect("pool");
    let err = sql!(&pool, "SELECT 1 AS one")
        .execute()
        .await
        .expect_err("unreachable");
    assert!(matches!(err, typedpg::Error::Pool(_)), "deadpool: {err:?}");

    let mut config = tokio_postgres::Config::new();
    config
        .host("127.0.0.1")
        .port(port)
        .user("postgres")
        .dbname("postgres");
    let pool = bb8::Pool::builder()
        .connection_timeout(Duration::from_millis(300))
        .build_unchecked(bb8_postgres::PostgresConnectionManager::new(config, NoTls));
    let err = sql!(&pool, "SELECT 1 AS one")
        .execute()
        .await
        .expect_err("unreachable");
    assert!(matches!(err, typedpg::Error::Pool(_)), "bb8: {err:?}");
}
