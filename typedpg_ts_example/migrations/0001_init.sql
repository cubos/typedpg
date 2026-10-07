CREATE TYPE mood AS ENUM ('happy', 'sad', 'neutral');
CREATE DOMAIN user_prefs AS jsonb;
CREATE TYPE address AS (street text, number int4);

CREATE TABLE users (
    id int4 GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name text NOT NULL,
    email text,
    mood mood,
    tags text[] NOT NULL DEFAULT '{}',
    prefs user_prefs,
    balance numeric(12, 2) NOT NULL DEFAULT 0,
    visits int8 NOT NULL DEFAULT 0,
    avatar bytea,
    home address,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE posts (
    id int4 GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    author_id int4 NOT NULL REFERENCES users (id),
    title text NOT NULL,
    body text
);
