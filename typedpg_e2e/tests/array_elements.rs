//! Arrays whose elements the analysis knows can be NULL read as
//! `Vec<Option<T>>`; the rest as `Vec<T>`.

mod common;

use typedpg::sql;
use typedpg_e2e::PostStatus;

#[tokio::test]
async fn array_agg_of_a_nullable_column_reads_null_elements() {
    let pool = common::setup().await;
    let name = &common::unique("elems");
    let (email_a, email_b) = (format!("{name}-a"), format!("{name}-b"));
    for (email, age) in [(&email_a, Some(30)), (&email_b, None)] {
        sql!(
            &pool,
            "INSERT INTO users (name, email, age) VALUES ($name, $email, $age)"
        )
        .execute()
        .await
        .expect("insert");
    }
    let row = sql!(
        &pool,
        "SELECT array_agg(age ORDER BY email) AS ages, array_agg(email ORDER BY email) AS emails, \
                ARRAY[min(age), NULL] AS pair, \
                ARRAY(SELECT age FROM users WHERE name = $name ORDER BY email) AS sub, \
                string_to_array('a,,b', ',', '') AS parts, regexp_match('b', '(a)|(b)') AS m \
         FROM users WHERE name = $name"
    )
    .fetch_one()
    .await
    .expect("aggregate");
    let ages: Option<Vec<Option<i32>>> = row.ages;
    assert_eq!(ages, Some(vec![Some(30), None]));
    let emails: Option<Vec<String>> = row.emails;
    assert_eq!(emails, Some(vec![email_a.clone(), email_b.clone()]));
    let pair: Vec<Option<i32>> = row.pair;
    assert_eq!(pair, vec![Some(30), None]);
    let sub: Vec<Option<i32>> = row.sub;
    assert_eq!(sub, vec![Some(30), None]);
    // NULL only for a NULL input string.
    let parts: Vec<Option<String>> = row.parts;
    assert_eq!(
        parts,
        vec![Some("a".to_string()), None, Some("b".to_string())]
    );
    let m: Option<Vec<Option<String>>> = row.m;
    assert_eq!(m, Some(vec![None, Some("b".to_string())]));
}

#[tokio::test]
async fn enum_arrays_with_null_elements() {
    let pool = common::setup().await;
    let row = sql!(
        &pool,
        r#"SELECT ARRAY['draft'::post_status, NULL] AS "statuses!""#
    )
    .fetch_one()
    .await
    .expect("arrays");
    let statuses: Vec<Option<PostStatus>> = row.statuses;
    assert_eq!(statuses, vec![Some(PostStatus::Draft), None]);
}
