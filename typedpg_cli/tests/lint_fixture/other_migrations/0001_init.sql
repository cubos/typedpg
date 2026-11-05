-- The second database's schema: the `db = other` query is analyzed against
-- it, not against the default database's tables.
CREATE TABLE things (
    id integer PRIMARY KEY,
    name text
);
