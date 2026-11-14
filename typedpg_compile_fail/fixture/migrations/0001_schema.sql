CREATE TYPE mood AS ENUM ('happy', 'sad');
CREATE DOMAIN prefs AS JSONB;
CREATE TYPE address AS (street TEXT, city TEXT);

-- A domain over an enum array: an array of it is an array of arrays of an
-- enum, which the macro cannot decode.
CREATE DOMAIN moods AS mood[];

CREATE TABLE users (
    id    BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name  TEXT NOT NULL,
    email TEXT NOT NULL,
    age   INT,
    mood  mood,
    prefs prefs,
    addr  address,
    mood_history moods[]
);

-- Types whose parameters take a closed set of Rust types: some of their
-- Rust type's `From` impls would change the value's meaning silently.
CREATE TABLE digests (
    hash   BYTEA PRIMARY KEY,
    hashes BYTEA[] NOT NULL,
    doc    JSONB NOT NULL,
    docs   JSONB[] NOT NULL,
    day    DATE NOT NULL,
    days   DATE[] NOT NULL
);
