//! typedpg_core renders identifiers the way PG's `quote_identifier` does,
//! quoting the keywords that aren't unreserved. Its keyword list must match
//! the grammar this crate vendors.

#[test]
fn quote_identifier_quotes_exactly_the_non_unreserved_keywords() {
    let kwlist = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/libpg_query/src/postgres/include/parser/kwlist.h"
    ))
    .expect("the libpg_query submodule is checked out");
    let mut seen = 0;
    for line in kwlist.lines().filter(|l| l.starts_with("PG_KEYWORD(")) {
        let mut fields = line["PG_KEYWORD(".len()..].split(',').map(str::trim);
        let name = fields.next().unwrap().trim_matches('"');
        let category = fields.nth(1).unwrap();
        let quoted = category != "UNRESERVED_KEYWORD";
        let expected = if quoted {
            format!("\"{name}\"")
        } else {
            name.to_owned()
        };
        assert_eq!(
            typedpg_core::quote_identifier(name),
            expected,
            "{name} ({category})"
        );
        seen += 1;
    }
    assert!(seen > 400, "parsed only {seen} keywords from kwlist.h");
}
