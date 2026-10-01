//! Every built-in type the `sql!` macro maps to a Rust type round-trips:
//! bound as a parameter, read back as a column, as `None` for NULL, and as
//! an array (`type_matrix` is migration 0009).

mod common;

use std::ops::Bound;

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use rust_decimal::Decimal;
use serde_json::json;
use typedpg::sql;
use typedpg::types::{Interval, MultiRange, PgLsn, Range, TimeTz, Xid8};

/// One value of every mapped type, as the Rust type `sql!` maps it to.
#[allow(clippy::type_complexity)]
struct Values {
    c_bool: bool,
    c_bytea: Vec<u8>,
    c_char: i8,
    c_name: String,
    c_int2: i16,
    c_int4: i32,
    c_int8: i64,
    c_text: String,
    c_varchar: String,
    c_bpchar: String,
    c_oid: u32,
    c_xid: u32,
    c_cid: u32,
    c_xid8: Xid8,
    c_json: serde_json::Value,
    c_jsonb: serde_json::Value,
    c_cidr: cidr::IpCidr,
    c_inet: cidr::IpInet,
    c_macaddr: eui48::MacAddress,
    c_float4: f32,
    c_float8: f64,
    c_numeric: Decimal,
    c_date: NaiveDate,
    c_time: NaiveTime,
    c_timestamp: chrono::NaiveDateTime,
    c_timestamptz: DateTime<Utc>,
    c_interval: Interval,
    c_timetz: TimeTz,
    c_uuid: uuid::Uuid,
    c_pg_lsn: PgLsn,
    c_regproc: u32,
    c_regprocedure: u32,
    c_regoper: u32,
    c_regoperator: u32,
    c_regclass: u32,
    c_regtype: u32,
    c_regnamespace: u32,
    c_regrole: u32,
    c_regcollation: u32,
    c_regconfig: u32,
    c_regdictionary: u32,
    c_int4range: Range<i32>,
    c_int8range: Range<i64>,
    c_numrange: Range<Decimal>,
    c_daterange: Range<NaiveDate>,
    c_tsrange: Range<chrono::NaiveDateTime>,
    c_tstzrange: Range<DateTime<Utc>>,
    c_int4multirange: MultiRange<i32>,
    c_datemultirange: MultiRange<NaiveDate>,
}

fn values() -> Values {
    let date = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
    let time = NaiveTime::from_hms_micro_opt(13, 14, 15, 161_718).unwrap();
    let timestamp = date.and_time(time);
    let timestamptz = timestamp.and_utc();
    Values {
        c_bool: true,
        c_bytea: vec![0, 1, 254, 255],
        c_char: b'x' as i8,
        c_name: "a_name".into(),
        c_int2: -7,
        c_int4: 42,
        c_int8: i64::MAX,
        c_text: "some text".into(),
        c_varchar: "varchar".into(),
        c_bpchar: "abc".into(),
        c_oid: 26,
        c_xid: 4_000_000_000,
        c_cid: 7,
        c_xid8: Xid8(u64::MAX - 1),
        c_json: json!({"a": [1, 2]}),
        c_jsonb: json!(["b", null, true]),
        c_cidr: "10.1.0.0/16".parse().unwrap(),
        c_inet: "192.168.0.7/24".parse().unwrap(),
        c_macaddr: eui48::MacAddress::new([0x08, 0x00, 0x2b, 0x01, 0x02, 0x03]),
        c_float4: 1.5,
        c_float8: -2.25,
        c_numeric: Decimal::new(-1234567, 3),
        c_date: date,
        c_time: time,
        c_timestamp: timestamp,
        c_timestamptz: timestamptz,
        c_interval: Interval::new(14, -3, 4_000_005),
        c_timetz: TimeTz {
            time,
            offset: chrono::FixedOffset::west_opt(3 * 3600).unwrap(),
        },
        c_uuid: uuid::Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef),
        c_pg_lsn: PgLsn::from(0x16_B374_D848),
        // Any OID binds: the reg* types' binary input doesn't look them up.
        // These are now(), int4 + int4, pg_class, int4, pg_catalog, the
        // bootstrap superuser, the default collation and the `simple`
        // text search configuration and dictionary.
        c_regproc: 1299,
        c_regprocedure: 1299,
        c_regoper: 551,
        c_regoperator: 551,
        c_regclass: 1259,
        c_regtype: 23,
        c_regnamespace: 11,
        c_regrole: 10,
        c_regcollation: 100,
        c_regconfig: 3748,
        c_regdictionary: 3765,
        // Discrete ranges in PostgreSQL's canonical form: `[lower, upper)`.
        c_int4range: Range::new(Bound::Included(1), Bound::Excluded(10)),
        c_int8range: Range::new(Bound::Unbounded, Bound::Excluded(-5)),
        c_numrange: Range::new(
            Bound::Excluded(Decimal::new(15, 1)),
            Bound::Included(Decimal::new(25, 1)),
        ),
        c_daterange: Range::Empty,
        c_tsrange: Range::new(Bound::Included(timestamp), Bound::Unbounded),
        c_tstzrange: Range::new(
            Bound::Excluded(timestamptz),
            Bound::Included(timestamptz + chrono::TimeDelta::hours(1)),
        ),
        c_int4multirange: MultiRange(vec![
            Range::new(Bound::Included(1), Bound::Excluded(3)),
            Range::new(Bound::Included(5), Bound::Unbounded),
        ]),
        c_datemultirange: MultiRange(vec![]),
    }
}

/// Runs `$check!(field)` for every field of [`Values`].
macro_rules! for_each_field {
    ($check:ident) => {
        $check!(
            c_bool,
            c_bytea,
            c_char,
            c_name,
            c_int2,
            c_int4,
            c_int8,
            c_text,
            c_varchar,
            c_bpchar,
            c_oid,
            c_xid,
            c_cid,
            c_xid8,
            c_json,
            c_jsonb,
            c_cidr,
            c_inet,
            c_macaddr,
            c_float4,
            c_float8,
            c_numeric,
            c_date,
            c_time,
            c_timestamp,
            c_timestamptz,
            c_interval,
            c_timetz,
            c_uuid,
            c_pg_lsn,
            c_regproc,
            c_regprocedure,
            c_regoper,
            c_regoperator,
            c_regclass,
            c_regtype,
            c_regnamespace,
            c_regrole,
            c_regcollation,
            c_regconfig,
            c_regdictionary,
            c_int4range,
            c_int8range,
            c_numrange,
            c_daterange,
            c_tsrange,
            c_tstzrange,
            c_int4multirange,
            c_datemultirange
        )
    };
}

#[tokio::test]
async fn every_mapped_type_roundtrips_as_param_and_column() {
    let pool = common::setup().await;
    let Values {
        c_bool,
        c_bytea,
        c_char,
        c_name,
        c_int2,
        c_int4,
        c_int8,
        c_text,
        c_varchar,
        c_bpchar,
        c_oid,
        c_xid,
        c_cid,
        c_xid8,
        c_json,
        c_jsonb,
        c_cidr,
        c_inet,
        c_macaddr,
        c_float4,
        c_float8,
        c_numeric,
        c_date,
        c_time,
        c_timestamp,
        c_timestamptz,
        c_interval,
        c_timetz,
        c_uuid,
        c_pg_lsn,
        c_regproc,
        c_regprocedure,
        c_regoper,
        c_regoperator,
        c_regclass,
        c_regtype,
        c_regnamespace,
        c_regrole,
        c_regcollation,
        c_regconfig,
        c_regdictionary,
        c_int4range,
        c_int8range,
        c_numrange,
        c_daterange,
        c_tsrange,
        c_tstzrange,
        c_int4multirange,
        c_datemultirange,
    } = values();
    let id = common::unique_id(&pool).await;
    let row = sql!(
        &pool,
        "INSERT INTO type_matrix VALUES ($id, $c_bool, $c_bytea, $c_char, $c_name, $c_int2, \
         $c_int4, $c_int8, $c_text, $c_varchar, $c_bpchar, $c_oid, $c_xid, $c_cid, $c_xid8, \
         $c_json, $c_jsonb, $c_cidr, $c_inet, $c_macaddr, $c_float4, $c_float8, $c_numeric, \
         $c_date, $c_time, $c_timestamp, $c_timestamptz, $c_interval, $c_timetz, $c_uuid, \
         $c_pg_lsn, $c_regproc, $c_regprocedure, $c_regoper, $c_regoperator, $c_regclass, \
         $c_regtype, $c_regnamespace, $c_regrole, $c_regcollation, $c_regconfig, \
         $c_regdictionary, $c_int4range, $c_int8range, $c_numrange, $c_daterange, $c_tsrange, \
         $c_tstzrange, $c_int4multirange, $c_datemultirange) \
         RETURNING *"
    )
    .fetch_one()
    .await
    .expect("insert every type");

    let expected = values();
    macro_rules! check {
        ($($f:ident),*) => {
            $(assert_eq!(row.$f, Some(expected.$f), stringify!($f));)*
        };
    }
    for_each_field!(check);

    // The text form PostgreSQL reads back is the value that was bound.
    let text = sql!(
        &pool,
        "SELECT c_char::text AS c_char, c_xid::text AS c_xid, c_regclass::text AS c_regclass, \
         c_regoperator::text AS c_regoperator, c_interval::text AS c_interval, \
         c_timetz::text AS c_timetz, c_pg_lsn::text AS c_pg_lsn, c_inet::text AS c_inet, \
         c_macaddr::text AS c_macaddr, c_int4multirange::text AS c_int4multirange \
         FROM type_matrix WHERE id = $id"
    )
    .fetch_one()
    .await
    .expect("text forms");
    assert_eq!(text.c_char.as_deref(), Some("x"));
    assert_eq!(text.c_xid.as_deref(), Some("4000000000"));
    assert_eq!(text.c_regclass.as_deref(), Some("pg_class"));
    assert_eq!(text.c_regoperator.as_deref(), Some("+(integer,integer)"));
    assert_eq!(
        text.c_interval.as_deref(),
        Some("1 year 2 mons -3 days +00:00:04.000005")
    );
    assert_eq!(text.c_timetz.as_deref(), Some("13:14:15.161718-03"));
    assert_eq!(text.c_pg_lsn.as_deref(), Some("16/B374D848"));
    assert_eq!(text.c_inet.as_deref(), Some("192.168.0.7/24"));
    assert_eq!(text.c_macaddr.as_deref(), Some("08:00:2b:01:02:03"));
    assert_eq!(text.c_int4multirange.as_deref(), Some("{[1,3),[5,)}"));
}

#[tokio::test]
async fn every_mapped_type_reads_null_as_none() {
    let pool = common::setup().await;
    let id = common::unique_id(&pool).await;
    sql!(&pool, "INSERT INTO type_matrix (id) VALUES ($id)")
        .execute()
        .await
        .expect("insert NULLs");
    let row = sql!(&pool, "SELECT * FROM type_matrix WHERE id = $id")
        .fetch_one()
        .await
        .expect("read NULLs");
    macro_rules! check {
        ($($f:ident),*) => {
            $(assert!(row.$f.is_none(), stringify!($f));)*
        };
    }
    for_each_field!(check);

    // A NULL bound through an Option parameter.
    let id = common::unique_id(&pool).await;
    let c_int4: Option<i32> = None;
    let c_regclass: Option<u32> = None;
    let c_interval: Option<Interval> = None;
    let row = sql!(
        &pool,
        "INSERT INTO type_matrix (id, c_int4, c_regclass, c_interval) \
         VALUES ($id, $c_int4, $c_regclass, $c_interval) RETURNING c_int4, c_regclass, c_interval"
    )
    .fetch_one()
    .await
    .expect("insert Option::None");
    assert_eq!(
        (row.c_int4, row.c_regclass, row.c_interval),
        (None, None, None)
    );
}

#[tokio::test]
async fn every_mapped_type_roundtrips_as_an_array_param() {
    let pool = common::setup().await;
    let v = values();
    let bools = vec![v.c_bool, false];
    let expected_bools = bools.clone();
    let byteas = vec![v.c_bytea.clone()];
    let expected_byteas = byteas.clone();
    let chars = vec![v.c_char];
    let expected_chars = chars.clone();
    let names = vec![v.c_name.clone()];
    let expected_names = names.clone();
    let int2s = vec![v.c_int2];
    let expected_int2s = int2s.clone();
    let int4s = vec![v.c_int4, 0];
    let expected_int4s = int4s.clone();
    let int8s = vec![v.c_int8];
    let expected_int8s = int8s.clone();
    let texts = vec![v.c_text.clone()];
    let expected_texts = texts.clone();
    let oids = vec![v.c_oid];
    let expected_oids = oids.clone();
    let xids = vec![v.c_xid];
    let expected_xids = xids.clone();
    let cids = vec![v.c_cid];
    let expected_cids = cids.clone();
    let xid8s = vec![v.c_xid8];
    let expected_xid8s = xid8s.clone();
    let jsons = vec![v.c_json.clone()];
    let expected_jsons = jsons.clone();
    let jsonbs = vec![v.c_jsonb.clone()];
    let expected_jsonbs = jsonbs.clone();
    let cidrs = vec![v.c_cidr];
    let expected_cidrs = cidrs.clone();
    let inets = vec![v.c_inet];
    let expected_inets = inets.clone();
    let macaddrs = vec![v.c_macaddr];
    let expected_macaddrs = macaddrs.clone();
    let float4s = vec![v.c_float4];
    let expected_float4s = float4s.clone();
    let float8s = vec![v.c_float8];
    let expected_float8s = float8s.clone();
    let numerics = vec![v.c_numeric];
    let expected_numerics = numerics.clone();
    let dates = vec![v.c_date];
    let expected_dates = dates.clone();
    let times = vec![v.c_time];
    let expected_times = times.clone();
    let timestamps = vec![v.c_timestamp];
    let expected_timestamps = timestamps.clone();
    let timestamptzs = vec![v.c_timestamptz];
    let expected_timestamptzs = timestamptzs.clone();
    let intervals = vec![v.c_interval];
    let expected_intervals = intervals.clone();
    let timetzs = vec![v.c_timetz];
    let expected_timetzs = timetzs.clone();
    let uuids = vec![v.c_uuid];
    let expected_uuids = uuids.clone();
    let lsns = vec![v.c_pg_lsn];
    let expected_lsns = lsns.clone();
    let regclasses = vec![v.c_regclass, v.c_regtype];
    let expected_regclasses = regclasses.clone();
    let regtypes = vec![v.c_regtype];
    let expected_regtypes = regtypes.clone();
    let regprocs = vec![v.c_regproc];
    let expected_regprocs = regprocs.clone();
    let int4ranges = vec![v.c_int4range.clone(), Range::Empty];
    let expected_int4ranges = int4ranges.clone();
    let int4multiranges = vec![v.c_int4multirange.clone()];
    let expected_int4multiranges = int4multiranges.clone();
    let row = sql!(
        &pool,
        "SELECT $bools::bool[] AS bools, $byteas::bytea[] AS byteas, \
         $chars::\"char\"[] AS chars, $names::name[] AS names, $int2s::int2[] AS int2s, \
         $int4s::int4[] AS int4s, $int8s::int8[] AS int8s, $texts::text[] AS texts, \
         $oids::oid[] AS oids, $xids::xid[] AS xids, $cids::cid[] AS cids, \
         $xid8s::xid8[] AS xid8s, $jsons::json[] AS jsons, $jsonbs::jsonb[] AS jsonbs, \
         $cidrs::cidr[] AS cidrs, $inets::inet[] AS inets, $macaddrs::macaddr[] AS macaddrs, \
         $float4s::float4[] AS float4s, $float8s::float8[] AS float8s, \
         $numerics::numeric[] AS numerics, $dates::date[] AS dates, $times::time[] AS times, \
         $timestamps::timestamp[] AS timestamps, $timestamptzs::timestamptz[] AS timestamptzs, \
         $intervals::interval[] AS intervals, $timetzs::timetz[] AS timetzs, \
         $uuids::uuid[] AS uuids, $lsns::pg_lsn[] AS lsns, \
         $regclasses::regclass[] AS regclasses, $regtypes::regtype[] AS regtypes, \
         $regprocs::regproc[] AS regprocs, $int4ranges::int4range[] AS int4ranges, \
         $int4multiranges::int4multirange[] AS int4multiranges"
    )
    .fetch_one()
    .await
    .expect("array params");
    assert_eq!(row.bools, expected_bools);
    assert_eq!(row.byteas, expected_byteas);
    assert_eq!(row.chars, expected_chars);
    assert_eq!(row.names, expected_names);
    assert_eq!(row.int2s, expected_int2s);
    assert_eq!(row.int4s, expected_int4s);
    assert_eq!(row.int8s, expected_int8s);
    assert_eq!(row.texts, expected_texts);
    assert_eq!(row.oids, expected_oids);
    assert_eq!(row.xids, expected_xids);
    assert_eq!(row.cids, expected_cids);
    assert_eq!(row.xid8s, expected_xid8s);
    assert_eq!(row.jsons, expected_jsons);
    assert_eq!(row.jsonbs, expected_jsonbs);
    assert_eq!(row.cidrs, expected_cidrs);
    assert_eq!(row.inets, expected_inets);
    assert_eq!(row.macaddrs, expected_macaddrs);
    assert_eq!(row.float4s, expected_float4s);
    assert_eq!(row.float8s, expected_float8s);
    assert_eq!(row.numerics, expected_numerics);
    assert_eq!(row.dates, expected_dates);
    assert_eq!(row.times, expected_times);
    assert_eq!(row.timestamps, expected_timestamps);
    assert_eq!(row.timestamptzs, expected_timestamptzs);
    assert_eq!(row.intervals, expected_intervals);
    assert_eq!(row.timetzs, expected_timetzs);
    assert_eq!(row.uuids, expected_uuids);
    assert_eq!(row.lsns, expected_lsns);
    assert_eq!(row.regclasses, expected_regclasses);
    assert_eq!(row.regtypes, expected_regtypes);
    assert_eq!(row.regprocs, expected_regprocs);
    assert_eq!(row.int4ranges, expected_int4ranges);
    assert_eq!(row.int4multiranges, expected_int4multiranges);
}

#[tokio::test]
async fn void_reads_as_unit() {
    let pool = common::setup().await;
    let row = sql!(&pool, "SELECT pg_sleep(0) AS slept")
        .fetch_one()
        .await
        .expect("void column");
    let () = row.slept;
}
