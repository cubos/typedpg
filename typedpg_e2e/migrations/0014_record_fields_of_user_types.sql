-- Composite fields whose values carry an OID of the database's own, which
-- a record value's inline type does not resolve to a built-in: arrays of
-- an enum, a composite and a domain, a domain over an array, a user range
-- and an array of it, an array of a mapped domain over `json`, and an
-- unmapped domain over `jsonb`
-- (tests/composites.rs).

CREATE TYPE floatrange AS RANGE (subtype = float8);

CREATE DOMAIN int_list AS int[];

CREATE DOMAIN json_note AS json;

CREATE DOMAIN raw_doc AS jsonb;

CREATE TYPE exotic AS (
    statuses post_status[],
    addrs    address[],
    nums     positive_int[],
    list     int_list,
    span     floatrange,
    spans    floatrange[],
    notes    json_note[],
    doc      raw_doc
);

CREATE TABLE exotic_rows (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    e  exotic NOT NULL
);
