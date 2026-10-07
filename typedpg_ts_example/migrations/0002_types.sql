CREATE EXTENSION IF NOT EXISTS hstore;

CREATE DOMAIN email AS text CHECK (VALUE LIKE '%@%');
CREATE DOMAIN positive AS int4 CHECK (VALUE > 0);
CREATE DOMAIN tag_list AS text[];
CREATE TYPE color AS ENUM ('red', 'green', 'with space', 'quo"te', 'back\slash', 'NULL');
CREATE TYPE point2 AS (x float8, y float8);
CREATE TYPE shape AS (
    name text,
    color color,
    points point2[],
    tags text[],
    meta jsonb
);
CREATE DOMAIN shape_d AS shape;

CREATE TABLE things (
    id int4 GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    label text NOT NULL,
    email email,
    qty positive,
    tags tag_list,
    colors color[],
    shape shape,
    shape_d shape_d,
    shapes shape[],
    span int4range,
    periods tstzmultirange,
    amounts numrange,
    dates daterange,
    dur interval,
    attrs hstore,
    big int8,
    bigs int8[],
    nums numeric[],
    flags bool[],
    blobs bytea[],
    stamps timestamptz[],
    docs jsonb[],
    day date,
    at_local timestamp,
    clock timetz,
    id_uuid uuid,
    addr inet,
    money_v money,
    bits varbit,
    ch "char"
);
