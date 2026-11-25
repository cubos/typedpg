-- The jsonb columns json_stringify_param checks src/json.ts's calls against.
CREATE TABLE documents (
    id integer PRIMARY KEY,
    body jsonb NOT NULL,
    tags jsonb[]
);
