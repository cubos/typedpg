//! The SQL of a query with `$..` spreads, from the description `sql!`
//! generates for it and the number of items each spread has.

/// A part of a spread query's SQL.
#[doc(hidden)]
#[derive(Debug)]
pub enum SqlPiece {
    /// SQL as written, its parameters cast.
    Text(&'static str),
    /// The `(…)` list of `x IN $..list`: a placeholder per item, each
    /// followed by `cast`, or `empty` with no item.
    List {
        spread: usize,
        cast: &'static str,
        empty: &'static str,
    },
    /// A VALUES list holding rows spreads: `VALUES` and the items that have
    /// rows, or `empty` when none has.
    Values {
        items: &'static [ValuesItem],
        empty: &'static str,
    },
}

/// An item of a [`SqlPiece::Values`] list.
#[doc(hidden)]
#[derive(Debug)]
pub enum ValuesItem {
    /// A row written in the SQL (which may hold a list spread).
    Row(&'static [SqlPiece]),
    /// A row per item of a rows spread, a placeholder per field, each
    /// followed by its cast.
    Rows {
        spread: usize,
        casts: &'static [&'static str],
    },
}

/// The SQL of `pieces` for spreads of `sizes[i]` items, their placeholders
/// numbered from `first`, in the order they are written — the order
/// `sql!` binds their values in.
#[doc(hidden)]
pub fn spread_sql(pieces: &[SqlPiece], sizes: &[usize], first: usize) -> String {
    let mut sql = String::new();
    let mut next = first;
    write_pieces(&mut sql, pieces, sizes, &mut next);
    sql
}

fn write_pieces(sql: &mut String, pieces: &[SqlPiece], sizes: &[usize], next: &mut usize) {
    for piece in pieces {
        match piece {
            SqlPiece::Text(text) => sql.push_str(text),
            SqlPiece::List {
                spread,
                cast,
                empty,
            } => {
                if sizes[*spread] == 0 {
                    sql.push_str(empty);
                    continue;
                }
                sql.push('(');
                for i in 0..sizes[*spread] {
                    if i > 0 {
                        sql.push_str(", ");
                    }
                    placeholder(sql, next, cast);
                }
                sql.push(')');
            }
            SqlPiece::Values { items, empty } => {
                let has_rows = |item: &ValuesItem| match item {
                    ValuesItem::Row(_) => true,
                    ValuesItem::Rows { spread, .. } => sizes[*spread] > 0,
                };
                if !items.iter().any(has_rows) {
                    sql.push_str(empty);
                    continue;
                }
                sql.push_str("VALUES ");
                let mut first = true;
                for item in items.iter().filter(|item| has_rows(item)) {
                    if !first {
                        sql.push_str(", ");
                    }
                    first = false;
                    match item {
                        ValuesItem::Row(row) => write_pieces(sql, row, sizes, next),
                        ValuesItem::Rows { spread, casts } => {
                            for r in 0..sizes[*spread] {
                                if r > 0 {
                                    sql.push_str(", ");
                                }
                                sql.push('(');
                                for (c, cast) in casts.iter().enumerate() {
                                    if c > 0 {
                                        sql.push_str(", ");
                                    }
                                    placeholder(sql, next, cast);
                                }
                                sql.push(')');
                            }
                        }
                    }
                }
            }
        }
    }
}

fn placeholder(sql: &mut String, next: &mut usize, cast: &str) {
    sql.push('$');
    sql.push_str(&next.to_string());
    sql.push_str(cast);
    *next += 1;
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY: &str = "SELECT * FROM (VALUES (NULL::int4)) AS __typedpg_empty WHERE false";
    // `INSERT INTO t VALUES ($1), $..a { x }, $..b { x } RETURNING x IN $..c`
    const PIECES: &[SqlPiece] = &[
        SqlPiece::Text("INSERT INTO t "),
        SqlPiece::Values {
            items: &[
                ValuesItem::Row(&[SqlPiece::Text("($1::int4)")]),
                ValuesItem::Rows {
                    spread: 0,
                    casts: &["::int4"],
                },
                ValuesItem::Rows {
                    spread: 1,
                    casts: &["::int4"],
                },
            ],
            empty: EMPTY,
        },
        SqlPiece::Text(" RETURNING x IN "),
        SqlPiece::List {
            spread: 2,
            cast: "::int4",
            empty: "(SELECT NULL::int4 WHERE false)",
        },
    ];

    #[test]
    fn writes_the_items_that_have_rows() {
        assert_eq!(
            spread_sql(PIECES, &[2, 1, 2], 2),
            "INSERT INTO t VALUES ($1::int4), ($2::int4), ($3::int4), ($4::int4) \
             RETURNING x IN ($5::int4, $6::int4)"
        );
        // An empty spread leaves no comma; an empty list is a subquery.
        assert_eq!(
            spread_sql(PIECES, &[0, 1, 0], 2),
            "INSERT INTO t VALUES ($1::int4), ($2::int4) RETURNING x IN (SELECT NULL::int4 WHERE false)"
        );
    }

    #[test]
    fn a_list_of_empty_spreads_is_a_select_of_no_row() {
        const ONLY_SPREADS: &[SqlPiece] = &[
            SqlPiece::Text("INSERT INTO t "),
            SqlPiece::Values {
                items: &[
                    ValuesItem::Rows {
                        spread: 0,
                        casts: &["::int4"],
                    },
                    ValuesItem::Rows {
                        spread: 1,
                        casts: &["::int4"],
                    },
                ],
                empty: EMPTY,
            },
        ];
        assert_eq!(
            spread_sql(ONLY_SPREADS, &[0, 0], 1),
            format!("INSERT INTO t {EMPTY}")
        );
        assert_eq!(
            spread_sql(ONLY_SPREADS, &[0, 1], 1),
            "INSERT INTO t VALUES ($1::int4)"
        );
    }
}
