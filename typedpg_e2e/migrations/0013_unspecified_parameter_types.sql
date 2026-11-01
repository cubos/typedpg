-- Parameter types whose OIDs are the database's own but whose binary
-- formats embed none — a domain over an enum, a mapped enum, an
-- extension's scalar type — which sql! sends with OID 0 ("unspecified"),
-- in one round trip (tests/round_trips.rs).

CREATE EXTENSION IF NOT EXISTS citext;

CREATE DOMAIN mood_dom AS mood;

CREATE TABLE unspecified_types (
    id     INT PRIMARY KEY,
    mood   mood_dom,
    status post_status,
    name   citext
);
