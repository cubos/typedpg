CREATE EXTENSION vector;

CREATE DOMAIN embedding AS halfvec(3);
CREATE DOMAIN embedding_text AS halfvec(3);
CREATE TYPE mood AS ENUM ('happy', 'sad');
CREATE TYPE address AS (street text, number int4);
CREATE TYPE short_address AS (street text, number int4);
CREATE DOMAIN user_id AS int8;
CREATE DOMAIN prefs AS jsonb;
