-- Schema for tests/nullability.rs: a view over an outer join, generated
-- columns over nullable and NOT NULL inputs, and a column holding another
-- table's row type (whose fields carry no NOT NULL as a value).

CREATE VIEW user_post_titles AS
SELECT u.id AS user_id, u.name, p.title
FROM users u
LEFT JOIN posts p ON p.user_id = u.id;

CREATE TABLE generated_values (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    label TEXT NOT NULL UNIQUE,
    base INT,
    doubled INT GENERATED ALWAYS AS (base * 2) STORED,
    label_length INT GENERATED ALWAYS AS (length(label)) STORED,
    tripled INT GENERATED ALWAYS AS (base * 3) VIRTUAL
);

CREATE TABLE row_parts (
    a INT NOT NULL,
    b TEXT NOT NULL
);

CREATE TABLE row_holders (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    label TEXT NOT NULL UNIQUE,
    part row_parts NOT NULL
);
