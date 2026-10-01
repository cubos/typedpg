-- copy_in! targets: arrays and scalars of an enum and of a jsonb domain,
-- whose binary COPY encoding needs the column's own type OIDs.
CREATE TABLE copy_targets (
    id INT PRIMARY KEY,
    status post_status,
    statuses post_status[] NOT NULL,
    pref user_preferences,
    prefs user_preferences[],
    tags TEXT[] NOT NULL,
    amount NUMERIC,
    doubled INT GENERATED ALWAYS AS (id * 2) STORED
);
