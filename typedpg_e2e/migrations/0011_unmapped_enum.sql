-- An enum with no [package.metadata.typedpg.types] mapping: sql! reads and
-- binds its labels as String (tests/enums.rs).
CREATE TYPE mood AS ENUM ('ok', 'sad');

CREATE TABLE moods (
    id INT PRIMARY KEY,
    mood mood,
    fixed mood NOT NULL DEFAULT 'ok'
);
