-- The tables the fixture's queries use.
CREATE TABLE users (
    id integer PRIMARY KEY,
    name text NOT NULL,
    email text
);

CREATE TABLE posts (
    id integer PRIMARY KEY,
    author_id integer,
    title text
);
