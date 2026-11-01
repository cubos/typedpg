//! A query goes to the server in one round trip: `sql!` sends its
//! parameter types with it (`query_typed`), on the unnamed statement,
//! instead of preparing it first — a built-in type by its OID; an enum, a
//! domain over one or an extension's scalar type, whose OIDs are the
//! database's own, with OID 0 ("unspecified"), for the server to infer from
//! the placeholder's cast. One whose parameters include an array of such a
//! type, or whose result columns include one, still works, sent untyped as
//! before.
//!
//! The client talks to the server through [`Wire`], a proxy that reads the
//! client's protocol messages: a round trip is one `Sync` (each ends an
//! extended-protocol exchange the client then waits on), and a prepared
//! statement is a `Parse` with a name.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_postgres::NoTls;
use typedpg::sql;
use typedpg::stream::TryStreamExt;

/// What the client sent through a [`Wire`].
#[derive(Default)]
struct Counts {
    /// `Sync` and simple `Query` messages: the client's round trips.
    round_trips: AtomicUsize,
    /// `Parse` messages naming a statement (a prepared one).
    named_parses: AtomicUsize,
    /// `Parse` messages of the unnamed statement.
    unnamed_parses: AtomicUsize,
    /// The parameter type OIDs of the last `Parse` (0: unspecified).
    last_parse_oids: Mutex<Vec<u32>>,
}

/// A client connected through a counting proxy.
struct Wire {
    client: tokio_postgres::Client,
    counts: Arc<Counts>,
}

/// What the client sent while running one query.
#[derive(Debug, PartialEq)]
struct Sent {
    round_trips: usize,
    named_parses: usize,
    unnamed_parses: usize,
}

/// One round trip, the unnamed statement parsed once.
const ONE_ROUND_TRIP: Sent = Sent {
    round_trips: 1,
    named_parses: 0,
    unnamed_parses: 1,
};

impl Wire {
    async fn connect() -> Wire {
        let server = common::server().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
        let port = listener.local_addr().expect("proxy address").port();
        let counts = Arc::new(Counts::default());
        let upstream = (server.host().to_string(), server.port());
        let proxy_counts = Arc::clone(&counts);
        tokio::spawn(async move {
            let (client_side, _) = listener.accept().await.expect("accept");
            let server_side = TcpStream::connect(upstream)
                .await
                .expect("connect upstream");
            let (mut from_client, mut to_client) = client_side.into_split();
            let (mut from_server, mut to_server) = server_side.into_split();
            tokio::spawn(async move {
                let _ = tokio::io::copy(&mut from_server, &mut to_client).await;
            });
            let mut pending: Vec<u8> = Vec::new();
            let mut startup = true;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = match from_client.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                // Counted before the server sees them: by the time the
                // client has its reply, its messages are counted.
                pending.extend_from_slice(&buf[..n]);
                count_messages(&mut pending, &mut startup, &proxy_counts);
                if to_server.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
        });

        let mut config = tokio_postgres::Config::new();
        config
            .host("127.0.0.1")
            .port(port)
            .user("postgres")
            .password("postgres")
            .dbname(common::DATABASE)
            .ssl_mode(tokio_postgres::config::SslMode::Disable);
        let (client, connection) = config.connect(NoTls).await.expect("connect via proxy");
        tokio::spawn(connection);
        Wire { client, counts }
    }

    /// What the client sends while `run` runs.
    async fn sent<F: std::future::Future<Output = T>, T>(&self, run: F) -> (T, Sent) {
        let before = self.snapshot();
        let out = run.await;
        let after = self.snapshot();
        (
            out,
            Sent {
                round_trips: after.round_trips - before.round_trips,
                named_parses: after.named_parses - before.named_parses,
                unnamed_parses: after.unnamed_parses - before.unnamed_parses,
            },
        )
    }

    /// The parameter type OIDs of the last `Parse` the client sent.
    fn parse_oids(&self) -> Vec<u32> {
        self.counts.last_parse_oids.lock().unwrap().clone()
    }

    fn snapshot(&self) -> Sent {
        Sent {
            round_trips: self.counts.round_trips.load(Ordering::SeqCst),
            named_parses: self.counts.named_parses.load(Ordering::SeqCst),
            unnamed_parses: self.counts.unnamed_parses.load(Ordering::SeqCst),
        }
    }

    /// A `type_matrix` id no other test uses.
    ///
    /// A simple query: a prepared statement's `Close` (and its `Sync`) is
    /// sent when the statement is dropped, after the call returns, and would
    /// be counted against the next query.
    async fn unique_id(&self) -> i32 {
        let messages = self
            .client
            .simple_query("SELECT nextval('e2e_unique_ids')")
            .await
            .expect("nextval");
        let id = messages
            .iter()
            .find_map(|m| match m {
                tokio_postgres::SimpleQueryMessage::Row(row) => row.get(0),
                _ => None,
            })
            .expect("a row");
        id.parse().expect("id fits an INT")
    }
}

/// Count the complete frontend messages at the start of `pending`, and
/// drop them. The first is the startup message, which has no type byte.
fn count_messages(pending: &mut Vec<u8>, startup: &mut bool, counts: &Counts) {
    loop {
        let header = if *startup { 4 } else { 5 };
        if pending.len() < header {
            return;
        }
        let len_at = header - 4;
        let len = u32::from_be_bytes(pending[len_at..header].try_into().unwrap()) as usize;
        let total = len_at + len;
        if pending.len() < total {
            return;
        }
        if *startup {
            *startup = false;
        } else {
            match pending[0] {
                b'S' | b'Q' => {
                    counts.round_trips.fetch_add(1, Ordering::SeqCst);
                }
                b'P' => {
                    // The statement's name, NUL-terminated, comes first.
                    let named = pending[5] != 0;
                    let counter = if named {
                        &counts.named_parses
                    } else {
                        &counts.unnamed_parses
                    };
                    *counts.last_parse_oids.lock().unwrap() = parse_oids(&pending[5..total]);
                    counter.fetch_add(1, Ordering::SeqCst);
                }
                _ => {}
            }
        }
        pending.drain(..total);
    }
}

/// The parameter type OIDs a `Parse` message's body declares: after the
/// statement's name and the query, both NUL-terminated, an `Int16` count
/// and that many `Int32` OIDs.
fn parse_oids(body: &[u8]) -> Vec<u32> {
    let mut rest = body;
    for _ in 0..2 {
        let nul = rest
            .iter()
            .position(|b| *b == 0)
            .expect("a NUL-terminated string");
        rest = &rest[nul + 1..];
    }
    let count = u16::from_be_bytes([rest[0], rest[1]]) as usize;
    rest[2..2 + 4 * count]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|oid| u32::from_be_bytes(*oid))
        .collect()
}

/// Parameters of many built-in types, bound and returned through the typed
/// path — each value, and each as NULL — and every fetch method, in one
/// round trip each.
#[tokio::test]
async fn built_in_parameters_go_in_one_round_trip() {
    let wire = Wire::connect().await;
    let client = &wire.client;

    let (one, sent) = wire
        .sent(sql!(client, "SELECT 1 AS one").fetch_value())
        .await;
    assert_eq!(one.expect("no parameters"), 1);
    assert_eq!(sent, ONE_ROUND_TRIP, "a query without parameters");

    let id = wire.unique_id().await;
    let date = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
    let timestamptz: DateTime<Utc> = date
        .and_hms_micro_opt(13, 14, 15, 161_718)
        .unwrap()
        .and_utc();
    let uuid = uuid::Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
    let numeric = Decimal::new(-1234567, 3);
    let (row, sent) = wire
        .sent(
            sql!(
                client,
                "INSERT INTO type_matrix \
                 (id, c_bool, c_bytea, c_int2, c_int4, c_int8, c_numeric, c_text, c_varchar, \
                  c_uuid, c_timestamptz, c_date, c_json, c_jsonb) \
                 VALUES ($id, $b, $bytes, $i2, $i4, $i8, $numeric, $text, $varchar, \
                  $uuid, $ts, $date, $json, $jsonb) \
                 RETURNING c_bool, c_bytea, c_int2, c_int4, c_int8, c_numeric, c_text, c_varchar, \
                  c_uuid, c_timestamptz, c_date, c_json, c_jsonb",
                b = Some(true),
                bytes = Some(vec![0u8, 1, 254, 255]),
                i2 = Some(-7i16),
                i4 = Some(42),
                i8 = Some(i64::MAX),
                numeric = Some(numeric),
                text = Some("some text".to_string()),
                varchar = Some("varchar".to_string()),
                uuid = Some(uuid),
                ts = Some(timestamptz),
                date = Some(date),
                json = Some(json!({"a": [1, 2]})),
                jsonb = Some(json!(["b", null, true])),
            )
            .fetch_one(),
        )
        .await;
    let row = row.expect("insert the values");
    assert_eq!(sent, ONE_ROUND_TRIP, "an INSERT of every type");
    assert_eq!(row.c_bool, Some(true));
    assert_eq!(row.c_bytea, Some(vec![0, 1, 254, 255]));
    assert_eq!(row.c_int2, Some(-7));
    assert_eq!(row.c_int4, Some(42));
    assert_eq!(row.c_int8, Some(i64::MAX));
    assert_eq!(row.c_numeric, Some(numeric));
    assert_eq!(row.c_text.as_deref(), Some("some text"));
    assert_eq!(row.c_varchar.as_deref(), Some("varchar"));
    assert_eq!(row.c_uuid, Some(uuid));
    assert_eq!(row.c_timestamptz, Some(timestamptz));
    assert_eq!(row.c_date, Some(date));
    assert_eq!(row.c_json, Some(json!({"a": [1, 2]})));
    assert_eq!(row.c_jsonb, Some(json!(["b", null, true])));

    let null_id = wire.unique_id().await;
    let (row, sent) = wire
        .sent(
            sql!(
                client,
                "INSERT INTO type_matrix \
                 (id, c_bool, c_bytea, c_int2, c_int4, c_int8, c_numeric, c_text, c_varchar, \
                  c_uuid, c_timestamptz, c_date, c_json, c_jsonb) \
                 VALUES ($id, $b, $bytes, $i2, $i4, $i8, $numeric, $text, $varchar, \
                  $uuid, $ts, $date, $json, $jsonb) \
                 RETURNING c_bool, c_bytea, c_int2, c_int4, c_int8, c_numeric, c_text, c_varchar, \
                  c_uuid, c_timestamptz, c_date, c_json, c_jsonb",
                id = null_id,
                b = None,
                bytes = None,
                i2 = None,
                i4 = None,
                i8 = None,
                numeric = None,
                text = None::<String>,
                varchar = None::<String>,
                uuid = None,
                ts = None,
                date = None,
                json = None,
                jsonb = None,
            )
            .fetch_one(),
        )
        .await;
    let row = row.expect("insert NULLs");
    assert_eq!(sent, ONE_ROUND_TRIP, "an INSERT of NULLs");
    assert!(row.c_bool.is_none() && row.c_bytea.is_none() && row.c_int2.is_none());
    assert!(row.c_int4.is_none() && row.c_int8.is_none() && row.c_numeric.is_none());
    assert!(row.c_text.is_none() && row.c_varchar.is_none() && row.c_uuid.is_none());
    assert!(row.c_timestamptz.is_none() && row.c_date.is_none());
    assert!(row.c_json.is_none() && row.c_jsonb.is_none());

    // Array parameters.
    let ids = vec![id, null_id];
    let texts = vec!["some text".to_string(), "other".to_string()];
    let (rows, sent) = wire
        .sent(
            sql!(
                client,
                "SELECT id FROM type_matrix WHERE id = ANY($ids) AND c_text = ANY($texts)",
                ids = ids.clone()
            )
            .fetch_all(),
        )
        .await;
    let rows = rows.expect("array parameters");
    assert_eq!(sent, ONE_ROUND_TRIP, "array parameters");
    assert_eq!(rows.iter().map(|r| r.id).collect::<Vec<_>>(), vec![id]);

    // execute: the rows affected, from the same single exchange.
    let (affected, sent) = wire
        .sent(
            sql!(
                client,
                "UPDATE type_matrix SET c_text = $text WHERE id = ANY($ids)",
                text = "updated",
                ids = ids.clone()
            )
            .execute(),
        )
        .await;
    assert_eq!(affected.expect("execute"), 2);
    assert_eq!(sent, ONE_ROUND_TRIP, "execute");

    // fetch_stream: one exchange, the rows read as they come.
    let (stream, sent) = wire
        .sent(
            sql!(
                client,
                "SELECT id FROM type_matrix WHERE id = ANY($ids) ORDER BY id",
                ids = ids.clone()
            )
            .fetch_stream(),
        )
        .await;
    let streamed: Vec<_> = stream
        .expect("fetch_stream")
        .try_collect()
        .await
        .expect("stream rows");
    assert_eq!(sent, ONE_ROUND_TRIP, "fetch_stream");
    assert_eq!(streamed.iter().map(|r| r.id).collect::<Vec<_>>(), {
        let mut sorted = ids.clone();
        sorted.sort();
        sorted
    });

    // fetch_optional of nothing.
    let missing = -1;
    let (none, sent) = wire
        .sent(sql!(client, "SELECT id FROM type_matrix WHERE id = $missing").fetch_optional())
        .await;
    assert!(none.expect("fetch_optional").is_none());
    assert_eq!(sent, ONE_ROUND_TRIP, "fetch_optional");
}

struct MatrixRow {
    id: i32,
    c_int4: Option<i32>,
}

/// A spread's placeholders are typed too: a list, a list bound as one
/// array above the threshold, and rows.
#[tokio::test]
async fn spreads_go_in_one_round_trip() {
    let wire = Wire::connect().await;
    let client = &wire.client;
    let mut rows = Vec::new();
    for i in 0..3 {
        rows.push(MatrixRow {
            id: wire.unique_id().await,
            c_int4: Some(i),
        });
    }

    let (inserted, sent) = wire
        .sent(
            sql!(
                client,
                "INSERT INTO type_matrix (id, c_int4) VALUES $..rows { id, c_int4 }"
            )
            .execute(),
        )
        .await;
    assert_eq!(inserted.expect("insert rows"), 3);
    assert_eq!(sent, ONE_ROUND_TRIP, "a rows spread");

    let ids: Vec<i32> = rows.iter().map(|r| r.id).collect();
    let (found, sent) = wire
        .sent(
            sql!(
                client,
                "SELECT c_int4 FROM type_matrix WHERE id IN $..ids ORDER BY c_int4"
            )
            .fetch_all(),
        )
        .await;
    let found = found.expect("a list spread");
    assert_eq!(sent, ONE_ROUND_TRIP, "a list spread");
    assert_eq!(
        found.iter().map(|r| r.c_int4).collect::<Vec<_>>(),
        vec![Some(0), Some(1), Some(2)]
    );

    // Above the threshold, the list is bound as one int4[].
    let many: Vec<i32> = ids.iter().copied().chain(-2000..-1).collect();
    assert!(many.len() > typedpg::__private::ARRAY_THRESHOLD);
    let (found, sent) = wire
        .sent(sql!(client, "SELECT id FROM type_matrix WHERE id IN $..many").fetch_all())
        .await;
    assert_eq!(found.expect("a list bound as an array").len(), 3);
    assert_eq!(sent, ONE_ROUND_TRIP, "a list bound as an array");

    // An empty list has no placeholder.
    let none: Vec<i32> = Vec::new();
    let (found, sent) = wire
        .sent(sql!(client, "SELECT id FROM type_matrix WHERE id IN $..none").fetch_all())
        .await;
    assert!(found.expect("an empty list").is_empty());
    assert_eq!(sent, ONE_ROUND_TRIP, "an empty list");
}

/// An enum's OID is the database's own: a query returning an enum column,
/// or with an enum array parameter (whose binary format embeds the
/// element's OID), is sent untyped, prepared first as before — and works.
#[tokio::test]
async fn enum_columns_and_enum_arrays_fall_back_to_preparing() {
    let wire = Wire::connect().await;
    let client = &wire.client;
    let id = wire.unique_id().await;

    let m = Some("sad".to_string());
    let (inserted, sent) = wire
        .sent(
            sql!(
                client,
                "INSERT INTO moods (id, mood) VALUES ($id, $m) RETURNING mood"
            )
            .fetch_one(),
        )
        .await;
    assert_eq!(
        inserted.expect("insert an enum").mood.as_deref(),
        Some("sad")
    );
    assert!(sent.round_trips >= 2, "prepared first: {sent:?}");
    assert!(sent.named_parses >= 1, "prepared first: {sent:?}");

    let moods = vec!["sad".to_string()];
    let (found, sent) = wire
        .sent(
            sql!(
                client,
                "SELECT id FROM moods WHERE id = $id AND mood = ANY($moods)"
            )
            .fetch_all(),
        )
        .await;
    assert_eq!(found.expect("an enum array").len(), 1);
    assert!(sent.named_parses >= 1, "prepared first: {sent:?}");
}

/// An enum result column, on a connection that has never seen the enum,
/// with built-in parameters: sent typed, `tokio_postgres` would look the
/// column's type up with a query of its own while the pipelined query's
/// rows fill the response channel, and the lookup's reply — behind them —
/// would never be read. Such a query is sent untyped, prepared first, and
/// returns every row. The timeout turns a regression into a failure
/// instead of a hung run.
#[tokio::test]
async fn enum_result_columns_fall_back_and_do_not_hang() {
    const ROWS: i32 = 2000;
    let wire = Wire::connect().await;
    let client = &wire.client;
    let limit = std::time::Duration::from_secs(20);

    let n = ROWS;
    let (rows, sent) = tokio::time::timeout(
        limit,
        wire.sent(
            sql!(
                client,
                "SELECT g AS id, 'sad'::mood AS mood FROM generate_series(1, $n) AS g"
            )
            .fetch_all(),
        ),
    )
    .await
    .expect("fetch_all hung on the enum column's type lookup");
    let rows = rows.expect("fetch_all");
    assert_eq!(rows.len(), ROWS as usize);
    assert!(rows.iter().all(|r| r.mood == "sad"));
    assert!(sent.named_parses >= 1, "prepared first: {sent:?}");
    assert!(sent.round_trips >= 2, "prepared first: {sent:?}");

    // An array of the enum is no more known to the client.
    let fresh = Wire::connect().await;
    let client = &fresh.client;
    let (streamed, sent) = tokio::time::timeout(
        limit,
        fresh.sent(async {
            sql!(
                client,
                "SELECT g AS id, ARRAY['ok', 'sad']::mood[] AS moods \
                 FROM generate_series(1, $n) AS g"
            )
            .fetch_stream()
            .await
            .expect("fetch_stream")
            .try_collect::<Vec<_>>()
            .await
        }),
    )
    .await
    .expect("fetch_stream hung on the enum array column's type lookup");
    assert_eq!(streamed.expect("stream rows").len(), ROWS as usize);
    assert!(sent.named_parses >= 1, "prepared first: {sent:?}");
}

/// A domain parameter is cast to its base type, so PostgreSQL infers the
/// base type for it: a domain over a built-in type goes typed, as that
/// type.
#[tokio::test]
async fn domain_parameters_go_as_their_base_type() {
    let wire = Wire::connect().await;
    let client = &wire.client;
    let tag = common::unique("domain");
    let email = format!("{tag}@example.com");
    let preferences = typedpg_e2e::UserPreferences {
        theme: "dark".into(),
        newsletter: true,
        daily_digest_limit: 3,
    };
    let (row, sent) = wire
        .sent(
            sql!(
                client,
                "INSERT INTO users (name, email, preferences) VALUES ($tag, $email, $preferences) \
                 RETURNING preferences",
                preferences = Some(preferences.clone()),
            )
            .fetch_one(),
        )
        .await;
    assert_eq!(
        row.expect("a JSONB domain parameter").preferences,
        Some(preferences)
    );
    assert_eq!(sent.named_parses, 0, "{sent:?}");
    assert_eq!(sent.round_trips, 1, "{sent:?}");
}

/// A row of NULLs in `unspecified_types`, at an id no other test uses.
async fn unspecified_types_row(wire: &Wire) -> i32 {
    let id = wire.unique_id().await;
    wire.client
        .simple_query(&format!("INSERT INTO unspecified_types (id) VALUES ({id})"))
        .await
        .expect("insert a row");
    id
}

/// An enum's OID is the database's own, but its binary format (the label)
/// embeds none: an enum parameter goes with OID 0, the server inferring
/// the type from the placeholder's cast — in one round trip, on the
/// unnamed statement, whether the enum is bound as a `String` (unmapped) or
/// as a mapped Rust enum, in a query returning built-in columns or run
/// through `execute`.
#[tokio::test]
async fn enum_parameters_go_unspecified_in_one_round_trip() {
    let wire = Wire::connect().await;
    let client = &wire.client;
    let id = wire.unique_id().await;

    // An INSERT through `execute`: `mood` unmapped, bound as a String.
    let m = Some("sad".to_string());
    let (inserted, sent) = wire
        .sent(sql!(client, "INSERT INTO moods (id, mood) VALUES ($id, $m)").execute())
        .await;
    assert_eq!(inserted.expect("insert an enum"), 1);
    assert_eq!(sent, ONE_ROUND_TRIP, "an INSERT through execute");
    assert_eq!(wire.parse_oids(), [23, 0]);

    // An UPDATE through `execute`.
    let m = Some("ok".to_string());
    let (updated, sent) = wire
        .sent(sql!(client, "UPDATE moods SET mood = $m WHERE id = $id").execute())
        .await;
    assert_eq!(updated.expect("update an enum"), 1);
    assert_eq!(sent, ONE_ROUND_TRIP, "an UPDATE through execute");
    assert_eq!(wire.parse_oids(), [0, 23]);

    // A SELECT returning built-in columns.
    let m = "ok".to_string();
    let (found, sent) = wire
        .sent(
            sql!(
                client,
                "SELECT id, mood::text AS label FROM moods WHERE id = $id AND mood = $m"
            )
            .fetch_all(),
        )
        .await;
    let found = found.expect("select by an enum");
    assert_eq!(sent, ONE_ROUND_TRIP, "an unmapped enum in a SELECT");
    assert_eq!(wire.parse_oids(), [23, 0]);
    assert_eq!(found.len(), 1);
    assert_eq!((found[0].id, found[0].label.as_str()), (id, "ok"));

    // A list spread of enums, below the threshold: unspecified per item.
    let moods = ["sad".to_string(), "ok".to_string()];
    let (found, sent) = wire
        .sent(
            sql!(
                client,
                "SELECT id FROM moods WHERE id = $id AND mood IN $..moods"
            )
            .fetch_all(),
        )
        .await;
    let found = found.expect("select by a list of enums");
    assert_eq!(sent, ONE_ROUND_TRIP, "a list spread of enums");
    assert_eq!(wire.parse_oids(), [23, 0, 0]);
    assert_eq!(found.iter().map(|r| r.id).collect::<Vec<_>>(), [id]);

    // A mapped enum (`post_status` → `PostStatus`).
    let id = unspecified_types_row(&wire).await;
    let s = Some(typedpg_e2e::PostStatus::Published);
    let (row, sent) = wire
        .sent(
            sql!(
                client,
                "UPDATE unspecified_types SET status = $s WHERE id = $id \
                 RETURNING id, status::text AS label"
            )
            .fetch_one(),
        )
        .await;
    let row = row.expect("update a mapped enum");
    assert_eq!(sent, ONE_ROUND_TRIP, "a mapped enum");
    assert_eq!(wire.parse_oids(), [0, 23]);
    assert_eq!((row.id, row.label.as_deref()), (id, Some("published")));

    let s = typedpg_e2e::PostStatus::Published;
    let (found, sent) = wire
        .sent(
            sql!(
                client,
                "SELECT id FROM unspecified_types WHERE status = $s AND id = $id"
            )
            .fetch_optional(),
        )
        .await;
    assert_eq!(
        found.expect("select by a mapped enum").map(|r| r.id),
        Some(id)
    );
    assert_eq!(sent, ONE_ROUND_TRIP, "a mapped enum in a SELECT");
    assert_eq!(wire.parse_oids(), [0, 23]);
}

/// A domain parameter is cast to its base type: a domain over an enum goes
/// as the enum, unspecified.
#[tokio::test]
async fn domains_over_enums_go_unspecified_in_one_round_trip() {
    let wire = Wire::connect().await;
    let client = &wire.client;
    let id = wire.unique_id().await;

    let m = Some("sad".to_string());
    let (row, sent) = wire
        .sent(
            sql!(
                client,
                "INSERT INTO unspecified_types (id, mood) VALUES ($id, $m) \
                 RETURNING id, mood::text AS label"
            )
            .fetch_one(),
        )
        .await;
    let row = row.expect("insert a domain over an enum");
    assert_eq!(sent, ONE_ROUND_TRIP, "a domain over an enum");
    assert_eq!(wire.parse_oids(), [23, 0]);
    assert_eq!((row.id, row.label.as_deref()), (id, Some("sad")));
}

/// An extension's scalar type (`citext`, mapped to `String`) has an OID of
/// the database's own and a binary format that embeds none: it goes
/// unspecified too — and the server compares it as `citext`, ignoring case.
#[tokio::test]
async fn extension_scalar_parameters_go_unspecified_in_one_round_trip() {
    let wire = Wire::connect().await;
    let client = &wire.client;
    let id = unspecified_types_row(&wire).await;
    let name = common::unique("Citext");

    let n = Some(name.clone());
    let (updated, sent) = wire
        .sent(
            sql!(
                client,
                "UPDATE unspecified_types SET name = $n WHERE id = $id"
            )
            .execute(),
        )
        .await;
    assert_eq!(updated.expect("update a citext"), 1);
    assert_eq!(sent, ONE_ROUND_TRIP, "a citext through execute");
    assert_eq!(wire.parse_oids(), [0, 23]);

    let n = name.to_uppercase();
    let (found, sent) = wire
        .sent(sql!(client, "SELECT id FROM unspecified_types WHERE name = $n").fetch_all())
        .await;
    let found = found.expect("select by a citext");
    assert_eq!(sent, ONE_ROUND_TRIP, "a citext in a SELECT");
    assert_eq!(wire.parse_oids(), [0]);
    assert_eq!(found.iter().map(|r| r.id).collect::<Vec<_>>(), [id]);
}

/// A value the server rejects fails the same way sent unspecified as sent
/// untyped (prepared first): the error comes from the same receive
/// function, `enum_recv`.
#[tokio::test]
async fn an_invalid_enum_value_fails_as_before() {
    let wire = Wire::connect().await;
    let client = &wire.client;
    let id = wire.unique_id().await;

    let m = Some("zz".to_string());
    let (result, sent) = wire
        .sent(sql!(client, "UPDATE moods SET mood = $m WHERE id = $id").execute())
        .await;
    let typed = match result.expect_err("an invalid label") {
        typedpg::Error::Database(e) => e,
        other => panic!("not a database error: {other:?}"),
    };
    assert_eq!(sent, ONE_ROUND_TRIP, "sent unspecified");
    assert_eq!(wire.parse_oids(), [0, 23]);

    // The untyped path, with the SQL and parameters `sql!` sends.
    let label = typedpg::__private::EnumString("zz".to_string());
    let (untyped, sent) = wire
        .sent(typedpg::__private::run_execute(
            client,
            "UPDATE moods SET mood = ($1::public.mood) WHERE id = ($2::pg_catalog.int4)",
            &[&label, &id],
            None,
        ))
        .await;
    let untyped = match untyped.expect_err("an invalid label") {
        typedpg::Error::Database(e) => e,
        other => panic!("not a database error: {other:?}"),
    };
    assert!(sent.named_parses >= 1, "prepared first: {sent:?}");

    for err in [&typed, &untyped] {
        let db = err.as_db_error().expect("a server error");
        assert_eq!(
            db.code(),
            &tokio_postgres::error::SqlState::INVALID_TEXT_REPRESENTATION
        );
        assert_eq!(db.message(), "invalid input value for enum mood: \"zz\"");
    }
}

/// A bound value whose `ToSql` refuses the type built for an unspecified
/// parameter (here, wanting the database's OID) fails nothing: no byte has
/// reached the server, and the query is sent untyped, prepared first.
#[tokio::test]
async fn a_value_refusing_the_unspecified_type_falls_back() {
    use tokio_postgres::types::{IsNull, ToSql, Type, to_sql_checked};

    #[derive(Debug)]
    struct WantsOid(String);

    impl ToSql for WantsOid {
        fn to_sql(
            &self,
            _: &Type,
            out: &mut typedpg::__private::bytes::BytesMut,
        ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
            out.extend_from_slice(self.0.as_bytes());
            Ok(IsNull::No)
        }

        fn accepts(ty: &Type) -> bool {
            ty.name() == "citext" && ty.oid() != 0
        }

        to_sql_checked!();
    }

    let wire = Wire::connect().await;
    let client = &wire.client;
    let id = unspecified_types_row(&wire).await;
    let name = common::unique("wants-oid");
    client
        .simple_query(&format!(
            "UPDATE unspecified_types SET name = '{name}' WHERE id = {id}"
        ))
        .await
        .expect("set the name");

    let value = WantsOid(name.to_uppercase());
    let citext = typedpg::__private::ParamType::Unspecified {
        schema: "public",
        name: "citext",
        kind: typedpg::__private::UnspecifiedKind::Simple,
    };
    let (rows, sent) = wire
        .sent(typedpg::__private::run_query(
            client,
            "SELECT id FROM unspecified_types WHERE name = ($1::public.citext)",
            &[&value],
            Some(&[citext]),
            Some(&[23]),
        ))
        .await;
    let rows = rows.expect("sent untyped");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, i32>(0), id);
    assert!(sent.named_parses >= 1, "prepared first: {sent:?}");
    assert_eq!(sent.unnamed_parses, 0, "nothing sent typed: {sent:?}");
}
